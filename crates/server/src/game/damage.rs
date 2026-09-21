use crate::entities::agent::AgentKey;
use crate::entities::combat::{CombatDamage, CombatElement};
use crate::entities::creature::CreatureAttackDamage;
use crate::entities::items::{Item, ItemFlag};
use crate::entities::player::Player;
use crate::entities::position::{ItemPlacement, Position};
use crate::game::TickCtx;
use crate::game::combat::weapon_skill;
use crate::game::config::GAME_CONFIG;
use crate::game::death;
use crate::game::events::BroadcastMessage;
use crate::game::item_action::check_decay;
use crate::game::random::Rolls;
use crate::persistence::items::ITEM_CONFIGS;

/// The target may no longer be in the map when this returns: a lethal hit reaps it. Anything
/// a caller still needs to do to the target must happen before the call.
pub fn apply_damage(
    ctx: &mut TickCtx,
    target: AgentKey,
    mut damage: CombatDamage,
    source: Option<AgentKey>,
) {
    if damage.element == CombatElement::Mana {
        drain_mana(ctx, target, damage, source);
        return;
    }

    let absorbed = ctx
        .map
        .get_agent(target)
        .filter(|agent| agent.conditions().is_magic_shielded(ctx.tick))
        .map_or(0, |agent| damage.value.min(agent.mana().current));
    if absorbed > 0 {
        drain_mana(
            ctx,
            target,
            CombatDamage {
                element: CombatElement::Mana,
                value: absorbed,
                blocked_shield: false,
                blocked_armor: false,
            },
            source,
        );
        damage.value -= absorbed;
        if damage.value == 0 {
            return;
        }
    }

    let Some(agent) = ctx.map.get_agent(target) else {
        return;
    };
    let Some(target_pos) = ctx.map.agent_position(target).cloned() else {
        return;
    };

    // TODO: apply element modifier

    let blood_type = agent.get_creature_kind().map(|c| c.blood_type.clone());
    let life = agent.life().current;
    let (survivable, records_participation) = if agent.is_creature() {
        (life, true)
    } else {
        (life.saturating_sub(1), false) // TODO: player death
    };

    damage.value = damage.value.min(survivable);
    if damage.value == 0 {
        if damage.blocked_shield || damage.blocked_armor {
            ctx.events.push(BroadcastMessage::DamageTaken {
                source,
                target,
                position: target_pos,
                blood_type,
                damage,
            });
        }
        return;
    }

    let element = damage.element;
    let applied = damage.value;

    if let Some(mut agent) = ctx.map.agent_mut(target) {
        agent.remove_life(applied);
        if records_participation && let Some(source) = source {
            agent.record_damage(source, applied);
        }
    }

    ctx.events.push(BroadcastMessage::DamageTaken {
        source,
        target,
        position: target_pos.clone(),
        blood_type,
        damage,
    });

    if matches!(element, CombatElement::Physical) {
        draw_blood(ctx, &target_pos, target);
    }

    if ctx
        .map
        .get_agent(target)
        .is_some_and(|agent| agent.life().current == 0)
    {
        death::reap(ctx, target, source);
    }
}

fn drain_mana(
    ctx: &mut TickCtx,
    target: AgentKey,
    mut damage: CombatDamage,
    source: Option<AgentKey>,
) {
    let Some(position) = ctx.map.agent_position(target).cloned() else {
        return;
    };
    let Some(mut agent) = ctx.map.agent_mut(target) else {
        return;
    };
    damage.value = damage.value.min(agent.mana().current);
    if damage.value == 0 {
        return;
    }
    let blood_type = agent.get_creature_kind().map(|c| c.blood_type.clone());
    agent.remove_mana(damage.value);

    ctx.events.push(BroadcastMessage::DamageTaken {
        source,
        target,
        position,
        blood_type,
        damage,
    });
}

pub fn get_player_base_damage(player: &Player, roll: &mut Rolls) -> (CombatElement, u32) {
    let level = player.level();
    let skill = weapon_skill(player);
    let min = get_min_damage(player.weapon_attack(), level, skill.value);
    let max = get_max_damage(player.weapon_attack(), level, skill.value);
    (player.weapon_element(), roll.damage_roll(min, max))
}

pub fn get_creature_base_damage(
    attk: &CreatureAttackDamage,
    roll: &mut Rolls,
) -> (CombatElement, u32) {
    (
        attk.element,
        roll.damage_roll(attk.value.min, attk.value.max),
    )
}

fn get_max_damage(attack_value: u16, level: u16, skill_value: u16) -> u32 {
    (((level as f32) / 5.5) + (((skill_value as f32) / 3.5) * ((attack_value as f32) / 3.0)))
        .round() as u32
}

fn get_min_damage(attack_value: u16, level: u16, skill_value: u16) -> u32 {
    (((level as f32) / 5.0) + (((skill_value as f32) / 10.0) * ((attack_value as f32) / 10.0)))
        .round() as u32
}

fn draw_blood(ctx: &mut TickCtx, attacked_pos: &Position, attacked_key: AgentKey) {
    let Some(config) = ITEM_CONFIGS.get(&GAME_CONFIG.combat.pool_item_id) else {
        return;
    };
    let Some(attacked) = ctx.map.get_agent(attacked_key) else {
        return;
    };
    let fluid = attacked.blood_type().get_fluid();

    let (ground_depth, existing) = ctx
        .map
        .iter_items(attacked_pos)
        .map(|items| {
            let items: Vec<_> = items.collect();
            let depth = items
                .iter()
                .take_while(|it| it.config.has_flag(ItemFlag::Ground))
                .count();
            let existing = items
                .iter()
                .find(|it| it.config.has_flag(ItemFlag::LiquidPool))
                .map(|it| it.guid);
            (depth, existing)
        })
        .unwrap_or((0, None));

    let pool = Item::new_fluid(config.clone(), fluid);
    let tick = ctx.tick;
    let Ok(item) = ctx
        .map
        .place_item(attacked_pos, Some(ground_depth), None, pool)
    else {
        return;
    };
    check_decay(
        ctx.scheduled,
        item,
        ItemPlacement::Map(attacked_pos.clone()),
        tick,
    );
    if let Some(guid) = existing {
        ctx.map.remove_item_from_tile(attacked_pos, &guid, 1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::combat::AttackPlan;
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::world_map::WorldMap;
    use crate::game::combat::plan_auto_attack;
    use crate::game::{TestHarness, Tick};
    use crate::persistence::test_fixtures::{a_test_creature, a_test_snapshot};

    fn physical(value: u32) -> CombatDamage {
        CombatDamage {
            element: CombatElement::Physical,
            value,
            blocked_shield: false,
            blocked_armor: false,
        }
    }

    fn fire(value: u32) -> CombatDamage {
        CombatDamage {
            element: CombatElement::Fire,
            value,
            blocked_shield: false,
            blocked_armor: false,
        }
    }

    fn map_with_creature(life: u32) -> (GameMap, AgentKey, Position) {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", life, (1, 2)), &pos)
            .unwrap();
        (map, rat, pos)
    }

    fn map_with_player() -> (GameMap, AgentKey) {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();
        (map, player)
    }

    fn blocked() -> CombatDamage {
        CombatDamage {
            element: CombatElement::Physical,
            value: 0,
            blocked_shield: true,
            blocked_armor: false,
        }
    }

    fn an_attacked_creature(life: u32) -> (GameMap, AgentKey, AgentKey) {
        let rat_pos = Position::new(10, 10, 7);
        let hunter_pos = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(rat_pos.clone(), MapTile::new());
        map.insert_tile(hunter_pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", life, (1, 2)), &rat_pos)
            .unwrap();
        let hunter = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &hunter_pos)
            .unwrap();
        (map, rat, hunter)
    }

    fn an_attacked_player() -> (GameMap, AgentKey, AgentKey) {
        let player_pos = Position::new(10, 10, 7);
        let rat_pos = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(player_pos.clone(), MapTile::new());
        map.insert_tile(rat_pos.clone(), MapTile::new());
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &player_pos)
            .unwrap();
        let rat = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &rat_pos)
            .unwrap();
        (map, player, rat)
    }

    fn reported_damage(msgs: &[BroadcastMessage]) -> Option<u32> {
        msgs.iter().find_map(|m| match m {
            BroadcastMessage::DamageTaken { damage, .. } => Some(damage.value),
            _ => None,
        })
    }

    #[test]
    fn overkill_reports_only_the_damage_that_landed() {
        let (map, rat, _) = map_with_creature(3);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, physical(50), None);

        assert_eq!(reported_damage(&h.events), Some(3));
    }

    #[test]
    fn a_lethal_hit_removes_the_creature() {
        let (map, rat, _) = map_with_creature(3);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, physical(50), None);

        assert!(map.get_agent(rat).is_none());
    }

    #[test]
    fn a_survivable_hit_leaves_the_creature_in_the_map() {
        let (map, rat, _) = map_with_creature(10);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, physical(3), None);

        assert_eq!(map.get_agent(rat).unwrap().life().current, 7);
        assert_eq!(reported_damage(&h.events), Some(3));
    }

    #[test]
    fn a_lethal_hit_on_a_player_leaves_one_life() {
        let (map, player) = map_with_player();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), player, physical(500), None);

        assert_eq!(map.get_agent(player).unwrap().life().current, 1);
        assert_eq!(reported_damage(&h.events), Some(99));
    }

    #[test]
    fn a_player_at_one_life_takes_no_further_damage() {
        let (map, player) = map_with_player();
        let mut h = TestHarness::seeded(1);
        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), player, physical(500), None);
        h.events.clear();

        apply_damage(&mut h.ctx(&mut map), player, physical(500), None);

        assert_eq!(map.get_agent(player).unwrap().life().current, 1);
        assert!(h.events.is_empty());
    }

    #[test]
    fn a_missing_target_emits_nothing() {
        let (mut map, rat, _) = map_with_creature(10);
        map.remove_agent(rat);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, physical(3), None);

        assert!(h.events.is_empty());
    }

    #[test]
    fn physical_damage_splashes_blood() {
        let (map, rat, pos) = map_with_creature(10);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, physical(3), None);

        let pooled = map
            .iter_items(&pos)
            .unwrap()
            .any(|it| it.config.id == GAME_CONFIG.combat.pool_item_id);
        assert!(pooled);
    }

    #[test]
    fn a_non_physical_element_does_not_splash() {
        let (map, rat, pos) = map_with_creature(10);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, fire(3), None);

        let pooled = map
            .iter_items(&pos)
            .unwrap()
            .any(|it| it.config.id == GAME_CONFIG.combat.pool_item_id);
        assert!(!pooled);
    }

    #[test]
    fn a_hit_records_the_attackers_participation() {
        let (map, rat, hunter) = an_attacked_creature(100);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, physical(30), Some(hunter));

        let table = map.get_agent(rat).unwrap().participation();
        assert_eq!(table.total(), 30);
        assert_eq!(table.shares(100), vec![(hunter, 100)]);
    }

    #[test]
    fn repeated_hits_from_one_attacker_accumulate() {
        let (map, rat, hunter) = an_attacked_creature(100);
        let mut h = TestHarness::seeded(1);
        let mut map = WorldMap::new(map);

        for _ in 0..3 {
            apply_damage(&mut h.ctx(&mut map), rat, physical(10), Some(hunter));
        }

        assert_eq!(map.get_agent(rat).unwrap().participation().total(), 30);
    }

    #[test]
    fn a_hit_with_no_source_records_nothing() {
        let (map, rat, _) = an_attacked_creature(100);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, physical(30), None);

        assert_eq!(map.get_agent(rat).unwrap().life().current, 70);
        assert_eq!(map.get_agent(rat).unwrap().participation().total(), 0);
    }

    #[test]
    fn a_fully_blocked_hit_records_nothing() {
        let (map, rat, hunter) = an_attacked_creature(100);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, blocked(), Some(hunter));

        assert_eq!(reported_damage(&h.events), Some(0));
        assert_eq!(map.get_agent(rat).unwrap().participation().total(), 0);
    }

    /// Players cannot die, so nothing would ever read their table — and with no idle
    /// clear it would grow an entry per creature that ever hit them.
    #[test]
    fn a_players_participation_table_is_never_written() {
        let (map, player, rat) = an_attacked_player();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), player, physical(30), Some(rat));

        assert_eq!(map.get_agent(player).unwrap().life().current, 70);
        assert_eq!(map.get_agent(player).unwrap().participation().total(), 0);
    }

    /// The overkill must not inflate the killer's share. A 500 roll into a creature with
    /// 60 life left counts as 60, so a contributor who dealt 40 of the 100 total keeps
    /// 40% of the pool rather than 40/540 of it.
    #[test]
    fn overkill_does_not_inflate_the_killers_share() {
        let rat_pos = Position::new(10, 10, 7);
        let first_pos = Position::new(11, 10, 7);
        let second_pos = Position::new(12, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(rat_pos.clone(), MapTile::new());
        map.insert_tile(first_pos.clone(), MapTile::new());
        map.insert_tile(second_pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(
                crate::persistence::test_fixtures::a_test_creature_worth("Rat", 100, (1, 2), 100),
                &rat_pos,
            )
            .unwrap();
        let first = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &first_pos)
            .unwrap();
        let second = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 2)), &second_pos)
            .unwrap();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), rat, physical(40), Some(first));

        apply_damage(&mut h.ctx(&mut map), rat, physical(500), Some(second));

        assert!(map.get_agent(rat).is_none());
        let ticks = |key| {
            map.get_player(key)
                .unwrap()
                .skills()
                .get(&crate::entities::skills::SkillType::Level)
                .unwrap()
                .current_ticks
        };
        assert_eq!(ticks(first), 40);
        assert_eq!(ticks(second), 60);
    }

    fn planned_damage(plan: &AttackPlan) -> Option<&CombatDamage> {
        plan.damage.first().map(|(_, damage)| damage)
    }

    fn duel(attacker: Agent, target: Agent) -> (GameMap, AgentKey, AgentKey) {
        let a = Position::new(10, 10, 7);
        let b = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());
        let attacker = map.insert_agent(attacker, &a).unwrap();
        let target = map.insert_agent(target, &b).unwrap();
        map.get_agent_mut(attacker)
            .unwrap()
            .set_target(Some(target), 0);
        (map, attacker, target)
    }

    /// Pins the `weapon_attack()` fallback the test above depends on.
    #[test]
    fn an_unarmed_player_still_deals_damage() {
        let (map, attacker, _) = duel(
            Agent::from_player(a_test_snapshot(1, 1)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert!(
            planned_damage(&plan).is_some_and(|damage| damage.value > 0),
            "unarmed swings must still hurt"
        );
        assert_eq!(get_min_damage(5, 1, 100), 5);
        assert_eq!(get_max_damage(5, 1, 100), 48);
    }

    use crate::actors::world::WorldCommand;
    use crate::entities::Bounds;
    use crate::entities::combat::AttackCost;
    use crate::entities::conditions::{ConditionSpec, SpecSchedule};
    use crate::game::TickDelta;
    use crate::game::combat::execute_attack;
    use smallvec::SmallVec;

    fn a_shielded_player(mana: u32) -> (GameMap, AgentKey) {
        let (mut map, player) = map_with_player();
        let agent = map.get_agent_mut(player).unwrap();
        let spent = agent.mana().current - mana;
        agent.remove_mana(spent);
        agent.conditions_mut().extend_magic_shield(Tick(1000));
        (map, player)
    }

    fn reported(msgs: &[BroadcastMessage]) -> Vec<(CombatElement, u32)> {
        msgs.iter()
            .filter_map(|m| match m {
                BroadcastMessage::DamageTaken { damage, .. } => {
                    Some((damage.element, damage.value))
                }
                _ => None,
            })
            .collect()
    }

    fn mana(value: u32) -> CombatDamage {
        CombatDamage {
            element: CombatElement::Mana,
            value,
            blocked_shield: false,
            blocked_armor: false,
        }
    }

    #[test]
    fn a_shielded_hit_comes_off_mana_and_is_reported_as_mana() {
        let (map, player) = a_shielded_player(100);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), player, fire(30), None);

        let agent = map.get_agent(player).unwrap();
        assert_eq!((agent.life().current, agent.mana().current), (100, 70));
        assert!(agent.conditions().is_magic_shielded(h.tick));
        assert_eq!(reported(&h.events), [(CombatElement::Mana, 30)]);
        assert!(map.delta().agent(player).mana());
    }

    #[test]
    fn a_hit_the_mana_cannot_cover_spills_into_life_and_ends_the_shield() {
        let (map, player) = a_shielded_player(20);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), player, fire(50), None);

        let agent = map.get_agent(player).unwrap();
        assert_eq!((agent.life().current, agent.mana().current), (70, 0));
        assert!(!agent.conditions().is_magic_shielded(h.tick));
        assert_eq!(
            reported(&h.events),
            [(CombatElement::Mana, 20), (CombatElement::Fire, 30)]
        );
    }

    #[test]
    fn an_unshielded_hit_takes_only_life() {
        let (map, player) = map_with_player();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), player, fire(30), None);

        let agent = map.get_agent(player).unwrap();
        assert_eq!((agent.life().current, agent.mana().current), (70, 100));
        assert_eq!(reported(&h.events), [(CombatElement::Fire, 30)]);
    }

    #[test]
    fn a_mana_hit_drains_mana_and_never_life() {
        let (map, player) = map_with_player();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_damage(&mut h.ctx(&mut map), player, mana(30), None);
        apply_damage(&mut h.ctx(&mut map), player, mana(500), None);

        let agent = map.get_agent(player).unwrap();
        assert_eq!((agent.life().current, agent.mana().current), (100, 0));
        assert_eq!(
            reported(&h.events),
            [(CombatElement::Mana, 30), (CombatElement::Mana, 70)]
        );
    }

    #[test]
    fn a_fully_absorbed_hit_still_applies_its_condition() {
        let (mut map, player, rat) = an_attacked_player();
        map.get_agent_mut(player)
            .unwrap()
            .conditions_mut()
            .extend_magic_shield(Tick(1000));
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        execute_attack(
            &mut h.ctx(&mut map),
            AttackPlan {
                attacker: rat,
                damage: SmallVec::from([(player, fire(10))]),
                cost: AttackCost::None,
                trains: None,
                missile: None,
                area_effect: None,
                missed: false,
                condition: Some(ConditionSpec {
                    element: CombatElement::Earth,
                    damage: Bounds { min: 40, max: 40 },
                    interval: TickDelta(80),
                    schedule: SpecSchedule::Decaying { start: None },
                    delayed: true,
                }),
            },
        );

        assert_eq!(map.get_agent(player).unwrap().life().current, 100);
        assert!(h.scheduled.iter().any(|s| matches!(
            s.command,
            WorldCommand::DamageOverTimeTick { agent_key, .. } if agent_key == player
        )));
    }
}
