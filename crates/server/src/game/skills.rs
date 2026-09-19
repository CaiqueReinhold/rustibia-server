use crate::entities::vocation::Vocation;
use crate::game::TickCtx;
use crate::{
    entities::{
        agent::AgentKey,
        skills::{SkillType, SkillValue},
    },
    game::{
        config::{GAME_CONFIG, SkillsConfig},
        events::BroadcastMessage,
    },
};

/// Total experience to reach `level`
fn exp_for_level(level: u16) -> u64 {
    let lv = level as i64;
    ((((lv - 6) * lv + 17) * lv - 12) / 6 * 100).max(0) as u64
}

fn geometric(base: u64, multiplier: f32, steps: i32) -> u64 {
    (((base as f64) * (multiplier as f64).powi(steps)).round() as u64).max(1)
}

/// What the step INTO `level` costs for `skill`, for this vocation.
pub fn required_ticks(vocation: Vocation, skill: &SkillType, level: u16) -> u64 {
    required_ticks_in(&GAME_CONFIG.skills, vocation, skill, level)
}

fn required_ticks_in(cfg: &SkillsConfig, vocation: Vocation, skill: &SkillType, level: u16) -> u64 {
    let mult = cfg.vocations.get(vocation);
    let weapon_steps = level as i32 - (cfg.min_level as i32 + 1);

    match skill {
        SkillType::Level => {
            exp_for_level(level).saturating_sub(exp_for_level(level.saturating_sub(1)))
        }
        SkillType::Magic => geometric(cfg.base.magic, mult.magic, level as i32 - 1),
        SkillType::Distance => geometric(cfg.base.distance, mult.distance, weapon_steps),
        SkillType::Sword | SkillType::Axe | SkillType::Club => {
            geometric(cfg.base.melee, mult.melee, weapon_steps)
        }
        SkillType::Shielding => geometric(cfg.base.shielding, mult.shielding, weapon_steps),
    }
}

fn bp_from(current_ticks: u64, current_cost: u64, next_cost: u64) -> u16 {
    if next_cost <= current_cost {
        return 10_000;
    }

    ((current_ticks as u128 * 10_000) / next_cost as u128).min(10_000) as u16
}

/// Progress toward the next level in hundredths of a percent, `0..=10_000`.
pub fn progress_bp(vocation: Vocation, skill: &SkillType, value: &SkillValue) -> u16 {
    progress_bp_in(&GAME_CONFIG.skills, vocation, skill, value)
}

fn progress_bp_in(
    cfg: &SkillsConfig,
    vocation: Vocation,
    skill: &SkillType,
    value: &SkillValue,
) -> u16 {
    bp_from(
        value.current_ticks,
        required_ticks_in(cfg, vocation, skill, value.value),
        required_ticks_in(cfg, vocation, skill, value.value.saturating_add(1)),
    )
}

pub fn total_experience(value: &SkillValue) -> u64 {
    exp_for_level(value.value).saturating_add(value.current_ticks)
}

/// Adds `ticks` of progress and reports how many levels were gained.
/// `required(level)` gives the cost of the step into that level.
fn advance(skill: &mut SkillValue, ticks: u64, required: impl Fn(u16) -> u64) -> u16 {
    skill.current_ticks = skill.current_ticks.saturating_add(ticks);

    let mut gained = 0u16;
    loop {
        let current_cost = required(skill.value);
        let next_cost = required(skill.value.saturating_add(1));

        if next_cost <= current_cost || skill.current_ticks < next_cost {
            break;
        }

        skill.current_ticks -= next_cost;
        skill.value = skill.value.saturating_add(1);
        gained = gained.saturating_add(1);
    }

    gained
}

pub fn tick_skill(ctx: &mut TickCtx, agent_key: AgentKey, skill: SkillType, ticks: u64) {
    let Some(mut player) = ctx.map.player_mut(agent_key) else {
        return;
    };
    let vocation = player.vocation();
    let Some(skill_value) = player.skill_mut(skill) else {
        return;
    };

    let gained = advance(skill_value, ticks, |level| {
        required_ticks(vocation, &skill, level)
    });
    if matches!(skill, SkillType::Level) && gained > 0 {
        level_up(ctx, agent_key, gained as i16);
    }

    if matches!(skill, SkillType::Level) {
        ctx.events.push(BroadcastMessage::ExperienceGained {
            agent_key,
            amount: ticks,
        });
    }
    if gained > 0 {
        ctx.events.push(BroadcastMessage::SkillUpgraded {
            agent_key,
            skill_type: skill,
            gained,
        });
    }
}

fn level_up(ctx: &mut TickCtx, agent_key: AgentKey, gained: i16) {
    let Some(mut agent) = ctx.map.agent_mut(agent_key) else {
        return;
    };
    let base_speed = agent.base_speed();
    let Some(mut player) = agent.player_mut() else {
        return;
    };
    let vocation = player.vocation();
    let capacity = player
        .capacity()
        .saturating_add_signed(vocation.capacity_on_level_up() * gained as i32);
    player.set_capacity(capacity);
    agent.set_base_speed(base_speed + gained as u16);
    agent.change_max_life(vocation.life_on_level_up() * gained as i32);
    agent.change_max_mana(vocation.mana_on_level_up() * gained as i32);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curves() -> SkillsConfig {
        serde_yaml::from_str(
            "
min_level: 10
base: { melee: 50, distance: 30, magic: 1600, shielding: 100 }
vocations:
  knight: { melee: 1.1, distance: 1.4, magic: 3.0, shielding: 1.1 }
  paladin: { melee: 1.2, distance: 1.1, magic: 1.4, shielding: 1.1 }
  sorcerer: { melee: 2.0, distance: 2.0, magic: 1.1, shielding: 1.5 }
  druid: { melee: 1.8, distance: 1.8, magic: 1.1, shielding: 1.5 }
",
        )
        .unwrap()
    }

    fn skill(value: u16, current_ticks: u64) -> SkillValue {
        SkillValue {
            value,
            current_ticks,
        }
    }

    /// A flat curve: every level costs the same. The "stopped rising" guard
    /// reads this as a ceiling, which is what the last test below asserts --
    /// so it cannot be used as a fixture for ordinary levelling.
    fn flat(_level: u16) -> u64 {
        100
    }

    /// Strictly rising, and cheap to reason about: level N costs N * 100.
    fn linear(level: u16) -> u64 {
        level as u64 * 100
    }

    /// Doubling from level 1: 100, 200, 400, 800...
    fn doubling(level: u16) -> u64 {
        100 * 2u64.pow(level.saturating_sub(1) as u32)
    }

    /// The case this was rebuilt for: one award worth several levels. Levelling
    /// once per call left the surplus above the threshold and handed the other
    /// levels out on later, unrelated calls.
    ///
    /// 1100 + 1200 + 1300 buys levels 11, 12 and 13, leaving 50 of the 3650.
    #[test]
    fn one_award_can_be_worth_several_levels() {
        let mut s = skill(10, 0);

        let gained = advance(&mut s, 3650, linear);

        assert_eq!(gained, 3);
        assert_eq!(s.value, 13);
        assert_eq!(
            s.current_ticks, 50,
            "the remainder carries into the next level"
        );
    }

    /// Landing exactly on the threshold is a level with nothing left over.
    #[test]
    fn hitting_the_threshold_exactly_levels_once_and_carries_nothing() {
        let mut s = skill(10, 600);

        let gained = advance(&mut s, 500, linear);

        assert_eq!(gained, 1, "600 + 500 is exactly the 1100 level 11 costs");
        assert_eq!(s.value, 11);
        assert_eq!(s.current_ticks, 0);
    }

    #[test]
    fn progress_short_of_the_threshold_gains_nothing() {
        let mut s = skill(10, 600);

        let gained = advance(&mut s, 400, linear);

        assert_eq!(gained, 0, "1000 of the 1100 level 11 costs");
        assert_eq!(s.value, 10);
        assert_eq!(s.current_ticks, 1000);
    }

    /// Each level costs more than the last, so one award buys fewer levels as
    /// the skill grows: 200 then 400 spends 600 of the 700 and leaves 100.
    #[test]
    fn a_rising_curve_slows_later_levels() {
        let mut s = skill(1, 0);

        let gained = advance(&mut s, 700, doubling);

        assert_eq!(gained, 2, "200 then 400; a third level would cost 800");
        assert_eq!(s.value, 3);
        assert_eq!(s.current_ticks, 100);
    }

    /// TFS's exit condition: a curve that stops rising means the skill is maxed.
    /// Without it a flat curve would consume the whole award one level at a time
    /// and, at zero cost, never terminate at all.
    #[test]
    fn a_curve_that_stops_rising_is_a_ceiling() {
        let mut s = skill(10, 0);

        let gained = advance(&mut s, u64::MAX, flat);

        assert_eq!(gained, 0, "flat from the start, so already at the ceiling");
        assert_eq!(s.value, 10);
    }

    /// Experience matches TFS's published table: 100 to reach level 2, 200 for
    /// level 3, and 4200 total to reach level 8.
    #[test]
    fn the_experience_curve_matches_tfs() {
        assert_eq!(exp_for_level(1), 0);
        assert_eq!(exp_for_level(2), 100);
        assert_eq!(exp_for_level(3), 200);
        assert_eq!(exp_for_level(8), 4200);
    }

    /// A weapon skill costs its base at the first level above the floor, then
    /// multiplies. Below the floor the signed exponent gives a fraction.
    #[test]
    fn a_weapon_skill_costs_its_base_at_the_floor() {
        assert_eq!(geometric(50, 2.0, 0), 50);
        assert_eq!(geometric(50, 2.0, 1), 100);
        assert_eq!(geometric(50, 2.0, 3), 400);
        assert_eq!(geometric(50, 2.0, -1), 25, "below the floor is a fraction");
    }

    /// Every vocation pays the same for the first level above the floor: the
    /// exponent is zero there, so the multiplier cannot show. Worth pinning,
    /// because a test written at that level looks like it compares vocations
    /// and compares nothing.
    #[test]
    fn the_first_level_above_the_floor_costs_the_same_for_everyone() {
        let cfg = curves();
        let base = required_ticks_in(&cfg, Vocation::Knight, &SkillType::Sword, 11);

        assert_eq!(base, 50);
        assert_eq!(
            required_ticks_in(&cfg, Vocation::Sorcerer, &SkillType::Sword, 11),
            base
        );
    }

    #[test]
    fn vocation_decides_what_a_skill_costs() {
        let cfg = curves();
        // Level 20, not 11. At the first level above the floor the exponent is
        // zero, so every vocation pays exactly `base` and the multiplier does
        // not show — the curves only diverge from level 12 onward.
        let knight_sword = required_ticks_in(&cfg, Vocation::Knight, &SkillType::Sword, 20);
        let sorcerer_sword = required_ticks_in(&cfg, Vocation::Sorcerer, &SkillType::Sword, 20);
        assert!(
            knight_sword < sorcerer_sword,
            "knight {knight_sword} should train sword cheaper than sorcerer {sorcerer_sword}"
        );

        let knight_magic = required_ticks_in(&cfg, Vocation::Knight, &SkillType::Magic, 5);
        let sorcerer_magic = required_ticks_in(&cfg, Vocation::Sorcerer, &SkillType::Magic, 5);
        assert!(
            sorcerer_magic < knight_magic,
            "sorcerer {sorcerer_magic} should train magic cheaper than knight {knight_magic}"
        );
    }

    /// Experience is the same climb for everyone.
    #[test]
    fn experience_does_not_vary_by_vocation() {
        for vocation in [Vocation::Knight, Vocation::Sorcerer] {
            assert_eq!(
                required_ticks(vocation, &SkillType::Level, 8),
                required_ticks(Vocation::Paladin, &SkillType::Level, 8)
            );
        }
    }

    #[test]
    fn no_progress_is_an_empty_bar_and_the_threshold_is_a_full_one() {
        assert_eq!(bp_from(0, 50, 55), 0);
        assert_eq!(bp_from(55, 50, 55), 10_000);
    }

    #[test]
    fn partial_progress_keeps_two_decimals() {
        assert_eq!(bp_from(27, 50, 55), 4909);
    }

    /// A curve that has stopped rising is `advance`'s ceiling condition. The bar
    /// has to read it the same way or a maxed skill draws as empty forever.
    #[test]
    fn a_ceiling_reads_as_a_full_bar() {
        assert_eq!(bp_from(0, 100, 100), 10_000);
        assert_eq!(bp_from(u64::MAX, 100, 90), 10_000);
    }

    #[test]
    fn ticks_past_the_threshold_clamp() {
        assert_eq!(bp_from(200, 50, 55), 10_000);
    }

    /// Above `u64::MAX / 10_000` a u64 multiply saturates and the ratio stops
    /// tracking `current_ticks` — the bar freezes part-way instead of filling.
    #[test]
    fn progress_past_the_u64_multiply_boundary_still_tracks() {
        let next_cost = 5_000_000_000_000_000;

        assert_eq!(bp_from(next_cost / 2, 1, next_cost), 5_000);
        assert_eq!(bp_from(next_cost / 100 * 98, 1, next_cost), 9_800);
    }

    /// Level 11 -> 12 costs a knight 55; the step into 11 cost 50. Measuring 27
    /// ticks against the wrong one gives 5400 instead of 4909.
    #[test]
    fn progress_is_measured_against_the_next_levels_cost() {
        let value = skill(11, 27);

        assert_eq!(
            progress_bp_in(&curves(), Vocation::Knight, &SkillType::Sword, &value),
            4909
        );
    }

    #[test]
    fn total_experience_is_the_levels_below_plus_the_current_one() {
        assert_eq!(total_experience(&skill(1, 0)), 0);
        assert_eq!(total_experience(&skill(8, 0)), 4200);
        assert_eq!(total_experience(&skill(8, 55)), 4255);
    }
}
