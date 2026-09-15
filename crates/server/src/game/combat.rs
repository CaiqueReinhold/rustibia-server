use smallvec::SmallVec;
use tracing::error;

use crate::{
    constants::combat::{AMMO_HIT_CEILING, THROWN_HIT_CEILING},
    entities::{
        agent::{Agent, AgentKey},
        combat::{AttackCost, AttackPlan, CombatDamage, CombatElement, WeaponType},
        creature::BloodType,
        effects::{AreaEffect, EffectId, Missile},
        inventory::InventorySlot,
        items::{ItemFlag, ItemRef},
        map::GameMap,
        player::Player,
        position::{ItemPlacement, Position},
        skills::SkillType,
        spells::{CastTarget, SpellAttack, SpellTargetMode},
    },
    game::{
        Tick, TickCtx,
        config::{Color, GAME_CONFIG},
        damage::{apply_damage, get_creature_base_damage, get_player_base_damage},
        events::BroadcastMessage,
        item_movement::remove_item_at,
        map_query::can_throw,
        random::Rolls,
        skills::tick_skill,
        spells::{
            SpellCastingDenyReason, consume_mana, resolve_area, resolve_spell_targets, roll_power,
        },
    },
};

#[derive(Debug)]
pub struct WeaponSkill {
    pub value: u16,
    pub trains: Option<SkillType>,
}

pub fn plan_auto_attack(
    map: &GameMap,
    attacker: AgentKey,
    roll: &mut Rolls,
    current_tick: Tick,
) -> Option<AttackPlan> {
    let agent = map.get_agent(attacker)?;
    let from = map.agent_position(attacker)?.clone();
    let target = agent.target()?;
    let to = map.agent_position(target)?.clone();

    if agent.next_auto_attack_tick > current_tick || agent.next_walk_tick > current_tick {
        return None;
    }

    if !from.is_within(&to, agent.attack_range() as u16) {
        return None;
    }

    if agent.is_fleeing() {
        return None;
    }

    if agent.get_player().is_some_and(|p| p.weapon_attack() == 0) {
        return None;
    }

    if agent.attack_range() > 1 && !can_throw(map, &from, &to, true) {
        return None;
    }

    let cost = match agent.get_player() {
        Some(player) => match player.weapon_type() {
            WeaponType::Bow | WeaponType::Crossbow => AttackCost::Item(ItemRef {
                guid: player.weapon_ammo()?.guid.clone(),
                placement: ItemPlacement::Inventory(InventorySlot::RightHand, attacker),
            }),
            WeaponType::Wand | WeaponType::Rod => {
                let mana_cost = player.weapon_mana_cost();
                if !agent.mana().can_afford(mana_cost) {
                    return None;
                }
                AttackCost::Mana(mana_cost)
            }
            _ => AttackCost::None,
        },
        None => AttackCost::None,
    };

    let trains = agent
        .get_player()
        .and_then(|player| weapon_skill(player).trains);

    let missed = match agent.get_player() {
        Some(player) if is_distance_weapon(player.weapon_type()) => {
            distance_hit_chance(player, from.distance(&to)) < roll.uniform(1, 100) as i32
        }
        _ => false,
    };

    let (targets, area_effect) = match agent.get_player().and_then(|p| p.weapon_area()) {
        Some((shape, effect_id)) => {
            let (mut targets, delta) = resolve_area(map, &to, shape.get_delta());
            targets.retain(|key| *key != attacker);
            (
                targets,
                Some(AreaEffect {
                    effect_id,
                    origin: to.clone(),
                    delta,
                }),
            )
        }
        None => (Vec::from([target]), None),
    };

    let mut damage = SmallVec::new();

    if !missed {
        let (element, mut value) = if agent.is_creature() {
            get_creature_base_damage(&agent.get_creature_kind()?.melee, roll)
        } else {
            get_player_base_damage(agent.get_player()?, roll)
        };

        for target in targets {
            let Some(target_agent) = map.get_agent(target) else {
                continue;
            };
            let is_blockable = matches!(element, CombatElement::Physical) && value > 0;
            if is_blockable {
                value = apply_shield(value, target_agent, roll);
            }
            let blocked_shield = is_blockable && value == 0;
            if is_blockable && !blocked_shield {
                value = apply_armor(value, target_agent, roll);
            }
            let blocked_armor = is_blockable && !blocked_shield && value == 0;

            damage.push((
                target,
                CombatDamage {
                    element,
                    value,
                    blocked_shield,
                    blocked_armor,
                },
            ));
        }
    }

    let missile = if let Some(player) = agent.get_player()
        && let Some(missile) = player.weapon().and_then(|weapon| {
            weapon.config.attr_missile_id().or_else(|| {
                player
                    .weapon_ammo()
                    .and_then(|it| it.config.attr_missile_id())
            })
        }) {
        let to_position = if missed {
            miss_position(map, &from, &to, roll)
        } else {
            to
        };

        Some(Missile {
            missile_id: missile,
            from,
            to: to_position,
        })
    } else {
        None
    };

    Some(AttackPlan {
        attacker,
        damage,
        cost,
        trains,
        missile,
        area_effect,
        missed,
    })
}

pub fn plan_spell_attack(
    map: &GameMap,
    attacker: AgentKey,
    roll: &mut Rolls,
    spell: &SpellAttack,
    cast_target: &CastTarget,
) -> Result<AttackPlan, SpellCastingDenyReason> {
    if matches!(spell.target, SpellTargetMode::Caster) {
        return Err(SpellCastingDenyReason::InvalidTarget);
    }

    let from = map
        .agent_position(attacker)
        .ok_or(SpellCastingDenyReason::InvalidState(
            attacker,
            "spell caster not found",
        ))?
        .clone();
    let player = map
        .get_player(attacker)
        .ok_or(SpellCastingDenyReason::InvalidState(
            attacker,
            "non player casting spell",
        ))?;

    let mut targets = resolve_spell_targets(map, attacker, &spell.target, cast_target)?;
    targets.keys.retain(|key| attacker != *key);

    let missile = spell
        .missile_id
        .zip(targets.aim.clone())
        .map(|(missile_id, to)| Missile {
            missile_id,
            from,
            to,
        });
    let area_effect = targets
        .delta
        .zip(targets.aim)
        .map(|(delta, origin)| AreaEffect {
            effect_id: spell.effect_id,
            origin,
            delta,
        });

    let value = roll_power(player, &spell.power, roll);
    let element = spell.element;
    let damage = targets
        .keys
        .into_iter()
        .map(|target| {
            (
                target,
                CombatDamage {
                    element,
                    value,
                    blocked_shield: false,
                    blocked_armor: false,
                },
            )
        })
        .collect();

    Ok(AttackPlan {
        attacker,
        damage,
        cost: AttackCost::None,
        trains: None,
        missile,
        area_effect,
        missed: false,
    })
}

pub fn execute_attack(ctx: &mut TickCtx, plan: AttackPlan) {
    match plan.cost {
        AttackCost::Item(item) => {
            if let Err(e) = remove_item_at(ctx, &item, 1) {
                error!("Failed to consume item({:?}) on attack: {}", item, e);
            };
        }
        AttackCost::Mana(mana_cost) => {
            consume_mana(ctx, plan.attacker, mana_cost);
        }
        AttackCost::None => {}
    }

    if let Some(missile) = plan.missile {
        let miss_pos = missile.to.clone();
        ctx.events
            .push(BroadcastMessage::MissileLaunched { missile });
        if plan.missed {
            ctx.events
                .push(BroadcastMessage::AttackMissed { position: miss_pos });
            return;
        }
    }

    for (target, dmg) in &plan.damage {
        if dmg.blocked_shield {
            tick_skill(ctx, *target, SkillType::Shielding, 1);
        }
    }

    let landed = plan
        .damage
        .as_ref()
        .iter()
        .any(|(_, damage)| damage.value > 0);

    if let Some(skill_type) = plan.trains
        && landed
    {
        tick_skill(ctx, plan.attacker, skill_type, 1);
    }

    if let Some(area) = plan.area_effect {
        ctx.events
            .push(BroadcastMessage::AreaEffectAppeared { area_effect: area });
    }

    for (target, dmg) in plan.damage.into_iter() {
        apply_damage(ctx, target, dmg, Some(plan.attacker));
    }
}

pub fn get_damage_visuals(
    damage: &CombatDamage,
    blood_type: Option<&BloodType>,
) -> (EffectId, Color) {
    if damage.blocked_shield {
        return (
            GAME_CONFIG.effect_ids.shield_hit,
            GAME_CONFIG.text_colors.lightblue,
        );
    }
    if damage.blocked_armor {
        return (
            GAME_CONFIG.effect_ids.armor_hit,
            GAME_CONFIG.text_colors.lightblue,
        );
    }
    let effect = match damage.element {
        CombatElement::Physical => match blood_type {
            Some(BloodType::Blood) => GAME_CONFIG.effect_ids.life_hit,
            Some(BloodType::Poison) => GAME_CONFIG.effect_ids.poison_hit,
            None => GAME_CONFIG.effect_ids.life_hit,
        },
        CombatElement::Ice => GAME_CONFIG.effect_ids.ice_hit,
        CombatElement::Earth => GAME_CONFIG.effect_ids.earth_hit,
        CombatElement::Energy => GAME_CONFIG.effect_ids.energy_hit,
        CombatElement::Fire => GAME_CONFIG.effect_ids.fire_hit,
        CombatElement::Holy => GAME_CONFIG.effect_ids.holy_hit,
        CombatElement::Death => GAME_CONFIG.effect_ids.death_hit,
    };
    let color = match damage.element {
        CombatElement::Physical => match blood_type {
            Some(BloodType::Blood) => GAME_CONFIG.text_colors.red,
            Some(BloodType::Poison) => GAME_CONFIG.text_colors.lightgreen,
            None => GAME_CONFIG.text_colors.red,
        },
        CombatElement::Ice => GAME_CONFIG.text_colors.skyblue,
        CombatElement::Earth => GAME_CONFIG.text_colors.lightgreen,
        CombatElement::Energy => GAME_CONFIG.text_colors.eletric_purple,
        CombatElement::Fire => GAME_CONFIG.text_colors.orange,
        CombatElement::Holy => GAME_CONFIG.text_colors.red,
        CombatElement::Death => GAME_CONFIG.text_colors.red,
    };
    (effect, color)
}

pub fn weapon_skill(player: &Player) -> WeaponSkill {
    let weapon = player.weapon_type();
    let Some(skill) = weapon.skill() else {
        return WeaponSkill {
            value: GAME_CONFIG.combat.unarmed_skill,
            trains: None,
        };
    };
    // A wand resolves against magic level but does not train it by firing; mana spent is what
    // trains magic, and `consume_mana` is where that happens.
    let trains = (!matches!(weapon, WeaponType::Wand | WeaponType::Rod)).then_some(skill);
    WeaponSkill {
        value: player.skill(skill),
        trains,
    }
}

// private

/// The nine tiles a missed shot can land on, the target's own included.
const MISS_OFFSETS: [(i32, i32); 9] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (0, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

fn is_distance_weapon(weapon: WeaponType) -> bool {
    matches!(
        weapon,
        WeaponType::Bow | WeaponType::Crossbow | WeaponType::Distance
    )
}

/// A shot's hit chance in percent, resolved off the **projectile** -- the ammunition a bow
/// fires, or the thrown weapon itself, which is its own ammunition. A bow or crossbow can
/// only have additive modifiers.
///
/// Can exceed 100 or fall below 0 (`devileye` carries -20), and both are meaningful against a
/// 1..=100 roll: an always-hit and an always-miss.
fn distance_hit_chance(player: &Player, distance: u16) -> i32 {
    let ammo = player.weapon_ammo();
    let projectile = ammo.or_else(|| player.weapon());

    // A projectile that names a flat `hit_chance` -- the viper and leaf stars, and nothing
    // else -- ignores both the skill and the range.
    let flat = projectile
        .and_then(|it| it.config.attr_hit_chance())
        .unwrap_or(0);
    let mut chance = if flat != 0 {
        flat as i32
    } else {
        let ceiling = projectile
            .and_then(|it| it.config.attr_max_hit_chance())
            .unwrap_or_else(
                || match projectile.and_then(|it| it.config.attr_ammo_type()) {
                    Some(_) => AMMO_HIT_CEILING,
                    None => THROWN_HIT_CEILING,
                },
            );
        tabled_hit_chance(ceiling, player.skill(SkillType::Distance), distance)
    };

    if ammo.is_some()
        && let Some(bonus) = player.weapon().and_then(|it| it.config.attr_hit_chance())
    {
        chance += bonus as i32;
    }
    chance
}

/// The reference tabulates a chance per (ceiling, range) pair, each entry a distance skill
/// capped at the value the entry stops rewarding and then scaled. **Only three ceilings have
/// a table**; every other value -- which is most of the catalogue, 76 through 96 -- is itself
/// the chance, flat, and neither skill nor range moves it.
fn tabled_hit_chance(max_hit_chance: u8, skill: u16, distance: u16) -> i32 {
    let capped = |cap: u16| skill.min(cap) as f32;
    match (max_hit_chance, distance) {
        (THROWN_HIT_CEILING, 1 | 5) => skill.min(74) as i32 + 1,
        (THROWN_HIT_CEILING, 2) => (capped(28) * 2.40) as i32 + 8,
        (THROWN_HIT_CEILING, 3) => (capped(45) * 1.55) as i32 + 6,
        (THROWN_HIT_CEILING, 4) => (capped(58) * 1.25) as i32 + 3,
        (THROWN_HIT_CEILING, 6) => (capped(90) * 0.80) as i32 + 3,
        (THROWN_HIT_CEILING, 7) => (capped(104) * 0.70) as i32 + 2,
        (AMMO_HIT_CEILING, 1 | 5) => (capped(74) * 1.20) as i32 + 1,
        (AMMO_HIT_CEILING, 2) => (capped(28) * 3.20) as i32,
        (AMMO_HIT_CEILING, 3) => skill.min(45) as i32 * 2,
        (AMMO_HIT_CEILING, 4) => (capped(58) * 1.55) as i32,
        (AMMO_HIT_CEILING, 6 | 7) => skill.min(90) as i32,
        (100, 1 | 5) => (capped(73) * 1.35) as i32 + 1,
        (100, 2) => (capped(30) * 3.20) as i32 + 4,
        (100, 3) => (capped(48) * 2.05) as i32 + 2,
        (100, 4) => (capped(65) * 1.50) as i32 + 2,
        (100, 6) => (capped(87) * 1.20) as i32 - 4,
        (100, 7) => (capped(90) * 1.10) as i32 + 1,
        // A range no table covers falls back to the flat `hit_chance`, which is zero here by
        // construction -- the table is only consulted when it is. Unreachable today: every
        // weapon range is 1..=7.
        (THROWN_HIT_CEILING | AMMO_HIT_CEILING | 100, _) => 0,
        (ceiling, _) => ceiling as i32,
    }
}

/// A missed shot scatters onto one of the nine tiles around its target if the
/// attacker was not standing next to it.
fn miss_position(map: &GameMap, from: &Position, to: &Position, roll: &mut Rolls) -> Position {
    if from.is_within(to, 1) {
        return to.clone();
    }
    let landable: Vec<Position> = MISS_OFFSETS
        .iter()
        .filter_map(|(dx, dy)| to.checked_offset(*dx, *dy))
        .filter(|pos| can_land_missile(map, pos))
        .collect();
    roll.category_roll(&landable)
        .cloned()
        .unwrap_or_else(|| to.clone())
}

fn can_land_missile(map: &GameMap, pos: &Position) -> bool {
    let Ok(items) = map.iter_items(pos) else {
        return false;
    };
    let mut has_ground = false;
    let mut blocked = false;
    for item in items {
        has_ground |= item.config.has_flag(ItemFlag::Ground);
        blocked |= item.config.has_flag(ItemFlag::Unpass) && item.config.has_flag(ItemFlag::Unmove);
    }
    has_ground && !blocked
}

fn apply_shield(base_attack_value: u32, target: &Agent, roll: &mut Rolls) -> u32 {
    let Some(defense_value) = target.defense() else {
        return base_attack_value;
    };
    let defended = roll.uniform(defense_value / 2, defense_value);
    base_attack_value.saturating_sub(defended)
}

fn apply_armor(base_attack_value: u32, target: &Agent, roll: &mut Rolls) -> u32 {
    let armor = target.armor() as u32;
    if armor == 0 {
        return base_attack_value;
    }
    base_attack_value.saturating_sub(roll.uniform(armor / 2, armor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::combat::AmmoType;
    use crate::entities::effects::MissileId;
    use crate::entities::items::{Item, ItemAttribute, ItemConfig, ItemFlag, ItemId};
    use crate::entities::map::MapTile;
    use crate::entities::skills::SkillValue;
    use crate::game::TestHarness;
    use crate::persistence::player::PlayerSnapshot;
    use crate::persistence::test_fixtures::{
        a_test_creature, a_test_creature_that_flees, a_test_creature_with_defences, a_test_snapshot,
    };
    use std::collections::HashSet;
    use std::sync::Arc;

    fn a_config(
        id: ItemId,
        flags: HashSet<ItemFlag>,
        attrs: Vec<ItemAttribute>,
    ) -> Arc<ItemConfig> {
        Arc::new(ItemConfig::new(
            id,
            "thing".to_string(),
            None,
            None,
            flags,
            attrs,
        ))
    }

    fn a_wand(mana_cost: u32) -> Item {
        Item::new(
            a_config(
                ItemId(1),
                HashSet::new(),
                vec![
                    ItemAttribute::WeaponType(WeaponType::Wand),
                    ItemAttribute::ManaCost(mana_cost),
                    ItemAttribute::WeaponAttack(10),
                ],
            ),
            1,
        )
    }

    fn a_weapon_with_attack(attack: u16) -> Item {
        Item::new(
            a_config(
                ItemId(8),
                HashSet::new(),
                vec![
                    ItemAttribute::WeaponType(WeaponType::Sword),
                    ItemAttribute::WeaponAttack(attack),
                ],
            ),
            1,
        )
    }

    fn a_bow(range: Option<u8>) -> Item {
        let mut attrs = vec![
            ItemAttribute::WeaponType(WeaponType::Bow),
            ItemAttribute::WeaponAttack(10),
        ];
        if let Some(range) = range {
            attrs.push(ItemAttribute::WeaponRange(range));
        }
        Item::new(a_config(ItemId(2), HashSet::new(), attrs), 1)
    }

    fn a_bow_with_missile(missile: MissileId) -> Item {
        Item::new(
            a_config(
                ItemId(7),
                HashSet::new(),
                vec![
                    ItemAttribute::WeaponType(WeaponType::Bow),
                    ItemAttribute::WeaponAttack(10),
                    ItemAttribute::MissileId(missile),
                ],
            ),
            1,
        )
    }

    fn an_arrow(missile: Option<MissileId>) -> Item {
        let mut attrs = vec![ItemAttribute::AmmoType(AmmoType::Arrow)];
        if let Some(missile) = missile {
            attrs.push(ItemAttribute::MissileId(missile));
        }
        Item::new(a_config(ItemId(3), HashSet::new(), attrs), 10)
    }

    fn an_arrow_with_attack(attack: u16) -> Item {
        Item::new(
            a_config(
                ItemId(9),
                HashSet::new(),
                vec![
                    ItemAttribute::AmmoType(AmmoType::Arrow),
                    ItemAttribute::WeaponAttack(attack),
                ],
            ),
            10,
        )
    }

    /// The flat `hit_chance` is what keeps this arrow out of the hit roll: the tests that
    /// use it are about the damage it deals, not about whether it arrives.
    fn an_arrow_of(element: CombatElement) -> Item {
        Item::new(
            a_config(
                ItemId(10),
                HashSet::new(),
                vec![
                    ItemAttribute::AmmoType(AmmoType::Arrow),
                    ItemAttribute::WeaponAttack(25),
                    ItemAttribute::WeaponElement(element),
                    ItemAttribute::HitChance(100),
                ],
            ),
            10,
        )
    }

    /// Carries a missile id as well: the tile a shot reaches is only observable through
    /// the plan's `Missile`, so an arrow with no missile has no flight to assert on.
    fn an_arrow_with_hit_chance(chance: i16) -> Item {
        Item::new(
            a_config(
                ItemId(12),
                HashSet::new(),
                vec![
                    ItemAttribute::AmmoType(AmmoType::Arrow),
                    ItemAttribute::WeaponAttack(25),
                    ItemAttribute::HitChance(chance),
                    ItemAttribute::MissileId(MissileId(1)),
                ],
            ),
            10,
        )
    }

    fn a_bow_with_hit_chance(bonus: i16) -> Item {
        Item::new(
            a_config(
                ItemId(13),
                HashSet::new(),
                vec![
                    ItemAttribute::WeaponType(WeaponType::Bow),
                    ItemAttribute::WeaponRange(5),
                    ItemAttribute::HitChance(bonus),
                ],
            ),
            1,
        )
    }

    fn a_ground_item() -> Item {
        Item::new(
            a_config(ItemId(14), HashSet::from([ItemFlag::Ground]), Vec::new()),
            1,
        )
    }

    fn a_wall_item() -> Item {
        Item::new(
            a_config(
                ItemId(15),
                HashSet::from([ItemFlag::Ground, ItemFlag::Unpass, ItemFlag::Unmove]),
                Vec::new(),
            ),
            1,
        )
    }

    fn a_bow_of(element: CombatElement) -> Item {
        Item::new(
            a_config(
                ItemId(11),
                HashSet::new(),
                vec![
                    ItemAttribute::WeaponType(WeaponType::Bow),
                    ItemAttribute::WeaponAttack(10),
                    ItemAttribute::WeaponElement(element),
                ],
            ),
            1,
        )
    }

    fn a_quiver_holding(arrow: Item) -> Item {
        let mut quiver = Item::new(
            a_config(
                ItemId(4),
                HashSet::from([ItemFlag::AmmoContainer]),
                Vec::new(),
            ),
            1,
        );
        quiver.content = Some(vec![arrow]);
        quiver
    }

    fn a_quiver_of_arrows() -> Item {
        a_quiver_holding(an_arrow(None))
    }

    fn armed(left: Option<Item>, right: Option<Item>) -> PlayerSnapshot {
        let mut snapshot = a_test_snapshot(1, 1);
        if let Some(item) = left {
            snapshot.inventory.insert(InventorySlot::LeftHand, item);
        }
        if let Some(item) = right {
            snapshot.inventory.insert(InventorySlot::RightHand, item);
        }
        snapshot
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

    /// An auto attack plans at most one damage entry, against its own target; an empty
    /// list is a miss.
    fn planned_damage(plan: &AttackPlan) -> Option<&CombatDamage> {
        plan.damage.first().map(|(_, damage)| damage)
    }

    /// The tile a planned shot reaches -- where it hit, or where a miss scattered to.
    fn missile_landing(plan: &AttackPlan) -> Position {
        plan.missile
            .as_ref()
            .expect("a shot in flight carries a missile")
            .to
            .clone()
    }

    #[test]
    fn an_unarmed_player_plans_a_free_attack() {
        let (map, attacker, target) = duel(
            Agent::from_player(a_test_snapshot(1, 1)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert!(matches!(plan.cost, AttackCost::None));
        assert_eq!(
            plan.damage.first().map(|(hit, _)| *hit),
            Some(target),
            "the plan names the agent it damages"
        );
    }

    #[test]
    fn a_wand_plans_a_mana_cost() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(Some(a_wand(20)), None)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert!(matches!(plan.cost, AttackCost::Mana(20)));
    }

    #[test]
    fn a_bow_with_a_quiver_plans_an_ammo_cost() {
        let quiver = a_quiver_of_arrows();
        let arrow_guid = quiver.content.as_ref().unwrap()[0].guid.clone();
        let (map, attacker, _) = duel(
            Agent::from_player(armed(Some(a_bow(None)), Some(quiver))),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert_eq!(
            plan.cost,
            AttackCost::Item(ItemRef {
                guid: arrow_guid,
                placement: ItemPlacement::Inventory(InventorySlot::RightHand, attacker),
            })
        );
    }

    #[test]
    fn a_bow_without_ammo_plans_nothing() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(Some(a_bow(None)), None)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none());
    }

    #[test]
    fn a_wand_without_mana_plans_nothing() {
        let mut snapshot = armed(Some(a_wand(500)), None);
        snapshot.mana.current = 10;
        let (map, attacker, _) = duel(
            Agent::from_player(snapshot),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none());
    }

    #[test]
    fn a_wand_with_exactly_its_mana_cost_still_fires() {
        let mut snapshot = armed(Some(a_wand(20)), None);
        snapshot.mana.current = 20;
        let (map, attacker, _) = duel(
            Agent::from_player(snapshot),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0))
            .expect("exactly enough mana must fire");

        assert!(matches!(plan.cost, AttackCost::Mana(20)));
    }

    #[test]
    fn a_wand_one_short_of_its_mana_cost_plans_nothing() {
        let mut snapshot = armed(Some(a_wand(20)), None);
        snapshot.mana.current = 19;
        let (map, attacker, _) = duel(
            Agent::from_player(snapshot),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none());
    }

    /// TFS refuses only the *melee* spell blocks while a monster flees, so a fleeing
    /// ranged monster keeps shooting. Every creature attack is melee today, so a fleeing
    /// creature plans nothing at all.
    #[test]
    fn a_fleeing_creature_plans_no_melee() {
        let (mut map, attacker, _) = duel(
            a_test_creature_that_flees("Rat", 10, (1, 2), 5),
            Agent::from_player(a_test_snapshot(1, 1)),
        );
        let mut roll = Rolls::new(1);
        assert!(
            plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_some(),
            "above its threshold the same creature swings"
        );

        map.get_agent_mut(attacker).unwrap().remove_life(6);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none());
    }

    /// The threshold is inclusive, as `runonhealth` is in the reference.
    #[test]
    fn a_creature_exactly_on_its_threshold_flees() {
        let (mut map, attacker, _) = duel(
            a_test_creature_that_flees("Rat", 10, (1, 2), 5),
            Agent::from_player(a_test_snapshot(1, 1)),
        );
        let mut roll = Rolls::new(1);
        map.get_agent_mut(attacker).unwrap().remove_life(4);
        assert!(
            plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_some(),
            "6 of 10"
        );

        map.get_agent_mut(attacker).unwrap().remove_life(1);

        assert!(
            plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none(),
            "5 of 10"
        );
    }

    #[test]
    fn an_unexpired_cooldown_plans_nothing() {
        let (mut map, attacker, _) = duel(
            Agent::from_player(a_test_snapshot(1, 1)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        map.get_agent_mut(attacker).unwrap().next_auto_attack_tick = Tick(40);
        let mut roll = Rolls::new(1);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(10)).is_none());
    }

    #[test]
    fn a_target_out_of_range_plans_nothing() {
        let a = Position::new(10, 10, 7);
        let b = Position::new(20, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());
        let attacker = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &a)
            .unwrap();
        let target = map
            .insert_agent(a_test_creature("Rat", 10, (1, 2)), &b)
            .unwrap();
        map.get_agent_mut(attacker)
            .unwrap()
            .set_target(Some(target), 0);
        let mut roll = Rolls::new(1);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none());
    }

    /// A ranged attacker three tiles away with an `Unpass` item on the line. A *missing*
    /// tile would not do: `GameMap::has_sight` ends in `.unwrap_or(true)`, so sight is
    /// clear across a hole in the map.
    #[test]
    fn a_blocked_line_of_sight_plans_nothing() {
        let a = Position::new(10, 10, 7);
        let b = Position::new(13, 10, 7);
        let wall = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());
        map.insert_tile(wall.clone(), MapTile::new());
        map.place_item(
            &wall,
            None,
            None,
            Item::new(
                a_config(ItemId(6), HashSet::from([ItemFlag::Unpass]), Vec::new()),
                1,
            ),
        )
        .unwrap();
        let attacker = map
            .insert_agent(
                Agent::from_player(armed(Some(a_bow(Some(5))), Some(a_quiver_of_arrows()))),
                &a,
            )
            .unwrap();
        let target = map
            .insert_agent(a_test_creature("Rat", 10, (1, 2)), &b)
            .unwrap();
        map.get_agent_mut(attacker)
            .unwrap()
            .set_target(Some(target), 0);
        let mut roll = Rolls::new(1);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none());
    }

    #[test]
    fn no_target_plans_nothing() {
        let (map, attacker, _) = duel(
            Agent::from_player(a_test_snapshot(1, 1)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut map = map;
        map.get_agent_mut(attacker).unwrap().set_target(None, 0);
        let mut roll = Rolls::new(1);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none());
    }

    #[test]
    fn a_target_absent_from_the_map_plans_nothing() {
        let (mut map, attacker, target) = duel(
            Agent::from_player(a_test_snapshot(1, 1)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        map.remove_agent(target);
        let mut roll = Rolls::new(1);

        assert!(plan_auto_attack(&map, attacker, &mut roll, Tick(0)).is_none());
    }

    /// Pins the broadcast order the spec calls load-bearing: cost, then skill, then damage.
    /// The `Magic` skill entry is required — `tick_skill` returns without emitting for a skill
    /// the player does not have, and `a_test_snapshot` carries only `Level`.
    ///
    /// The auto-attack cooldown is **not** asserted here: it is the one auto-attack-specific
    /// thing the executor lost when the spell planners joined it, and it is stamped by
    /// `systems::combat_system` instead. `a_swing_stamps_the_auto_attack_cooldown` is what
    /// covers it now.
    #[test]
    fn executing_spends_the_mana_in_the_order_the_spec_pins() {
        let mut snapshot = armed(Some(a_wand(20)), None);
        snapshot.skills.insert(
            SkillType::Magic,
            SkillValue {
                value: 20,
                current_ticks: 0,
            },
        );
        let (mut map, attacker, _) = duel(
            Agent::from_player(snapshot),
            a_test_creature("Rat", 100, (1, 2)),
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(7);
        let plan = plan_auto_attack(&map, attacker, &mut h.roll, Tick(7)).unwrap();

        execute_attack(&mut h.ctx(&mut map), plan);

        let agent = map.get_agent(attacker).unwrap();
        assert_eq!(agent.mana().current, 80);
        assert_eq!(
            broadcast_kinds(&h.events),
            ["mana", "skill", "damage", "blood"]
        );
    }

    /// `Distance` is its own weapon type but costs nothing to swing — it fell through the
    /// old consumption match's `=> {}` arm, and must fall through to `AttackCost::None` here.
    #[test]
    fn a_distance_weapon_plans_no_cost() {
        let spear = Item::new(
            a_config(
                ItemId(5),
                HashSet::new(),
                vec![
                    ItemAttribute::WeaponType(WeaponType::Distance),
                    ItemAttribute::WeaponAttack(10),
                ],
            ),
            1,
        );
        let (map, attacker, _) = duel(
            Agent::from_player(armed(Some(spear), None)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert!(matches!(plan.cost, AttackCost::None));
    }

    #[test]
    fn executing_spends_one_arrow() {
        let (mut map, attacker, _) = duel(
            Agent::from_player(armed(Some(a_bow(None)), Some(a_quiver_of_arrows()))),
            a_test_creature("Rat", 100, (1, 2)),
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(0);
        let plan = plan_auto_attack(&map, attacker, &mut h.roll, Tick(0)).unwrap();

        execute_attack(&mut h.ctx(&mut map), plan);

        let arrows = map
            .get_agent(attacker)
            .unwrap()
            .get_player()
            .unwrap()
            .inventory()
            .get(&InventorySlot::RightHand)
            .unwrap()
            .content
            .as_ref()
            .unwrap()[0]
            .amount;
        assert_eq!(arrows, 9);
    }

    /// The count drawn on the arrow stack comes from `UpdateContainer`, and the session
    /// only sends one for a guid it holds as an open container. Naming the arrow instead of
    /// the quiver dropped the message on the floor, freezing the number until the stack ran
    /// out — the last arrow took the whole-removal branch, which named the quiver correctly.
    #[test]
    fn executing_a_shot_updates_the_quiver_rather_than_the_arrow() {
        let quiver = a_quiver_of_arrows();
        let quiver_guid = quiver.guid.clone();
        let arrow_guid = quiver.content.as_ref().unwrap()[0].guid.clone();
        let (mut map, attacker, _) = duel(
            Agent::from_player(armed(Some(a_bow(None)), Some(quiver))),
            a_test_creature("Rat", 100, (1, 2)),
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(0);
        let plan = plan_auto_attack(&map, attacker, &mut h.roll, Tick(0)).unwrap();

        execute_attack(&mut h.ctx(&mut map), plan);

        let updated = h
            .events
            .iter()
            .find_map(|m| match m {
                BroadcastMessage::ContainerUpdated { item } => Some(item),
                _ => None,
            })
            .expect("spending an arrow must announce the container it came out of");
        assert_eq!(updated.guid, quiver_guid);
        assert_ne!(updated.guid, arrow_guid);
    }

    fn broadcast_kinds(msgs: &[BroadcastMessage]) -> Vec<&'static str> {
        msgs.iter()
            .map(|m| match m {
                BroadcastMessage::MissileLaunched { .. } => "missile",
                BroadcastMessage::AttackMissed { .. } => "miss",
                BroadcastMessage::PlayerManaUpdated { .. } => "mana",
                BroadcastMessage::SkillProgressUpdated { .. }
                | BroadcastMessage::SkillUpgraded { .. } => "skill",
                BroadcastMessage::ContainerUpdated { .. } => "ammo",
                BroadcastMessage::DamageTaken { .. } => "damage",
                BroadcastMessage::TileChanged { .. } => "blood",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn a_weapon_carrying_a_missile_id_plans_that_missile() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(
                Some(a_bow_with_missile(MissileId(37))),
                Some(a_quiver_of_arrows()),
            )),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert_eq!(plan.missile.map(|it| it.missile_id), Some(MissileId(37)));
    }

    #[test]
    fn a_missile_id_on_the_ammo_is_used_when_the_weapon_has_none() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(
                Some(a_bow(None)),
                Some(a_quiver_holding(an_arrow(Some(MissileId(42))))),
            )),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert_eq!(plan.missile.map(|it| it.missile_id), Some(MissileId(42)));
    }

    #[test]
    fn an_unarmed_player_plans_no_missile() {
        let (map, attacker, _) = duel(
            Agent::from_player(a_test_snapshot(1, 1)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert!(plan.missile.is_none());
    }

    #[test]
    fn an_unarmed_attack_does_not_copy_the_player() {
        let (mut map, attacker, _) = duel(
            Agent::from_player(a_test_snapshot(1, 1)),
            a_test_creature("Rat", 100, (1, 2)),
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(0);
        let plan = plan_auto_attack(&map, attacker, &mut h.roll, Tick(0)).unwrap();
        assert!(matches!(plan.cost, AttackCost::None) && plan.trains.is_none());

        let snapshot = map.clone();

        execute_attack(&mut h.ctx(&mut map), plan);

        assert!(std::ptr::eq(
            map.get_player(attacker).unwrap(),
            snapshot.get_player(attacker).unwrap()
        ));
    }

    #[test]
    fn an_attack_that_pays_a_cost_copies_the_player() {
        let (mut map, attacker, _) = duel(
            Agent::from_player(armed(Some(a_wand(5)), None)),
            a_test_creature("Rat", 100, (1, 2)),
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(0);
        let plan = plan_auto_attack(&map, attacker, &mut h.roll, Tick(0)).unwrap();

        let snapshot = map.clone();

        execute_attack(&mut h.ctx(&mut map), plan);

        assert!(!std::ptr::eq(
            map.get_player(attacker).unwrap(),
            snapshot.get_player(attacker).unwrap()
        ));
    }

    /// Zero has to come from the weapon: `weapon_attack()` falls back to **5** when no
    /// weapon is held, so an unarmed player is not a zero-damage one. An attack of 0
    /// zeroes both damage bounds whatever the skill, because it is a factor in each.
    #[test]
    fn a_zero_damage_hit_is_not_reported_as_a_block() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(Some(a_weapon_with_attack(0)), None)),
            a_test_creature("Rat", 10, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        let damage = planned_damage(&plan).expect("a melee swing always lands");
        assert!(matches!(damage.element, CombatElement::Physical));
        assert_eq!(damage.value, 0, "a zero-attack weapon deals nothing");
        assert!(!damage.blocked_shield);
        assert!(!damage.blocked_armor);
    }

    /// The shipped bow carries no `attack` at all — the arrow does (25). Reading
    /// only the weapon slot dropped every distance shot to the unarmed fallback of 5, which
    /// against a creature with any armour at all is a permanent block.
    #[test]
    fn a_bow_rolls_with_the_ammos_attack() {
        let player = Agent::from_player(armed(
            Some(a_bow(None)),
            Some(a_quiver_holding(an_arrow_with_attack(25))),
        ));

        assert_eq!(player.get_player().unwrap().weapon_attack(), 25);
    }

    /// The ammo wins over the weapon, rather than filling in for a weapon that has none:
    /// an `attack` that strays onto a bow must not override the arrow it fires.
    #[test]
    fn a_melee_weapon_still_rolls_with_its_own_attack() {
        let player = Agent::from_player(armed(Some(a_weapon_with_attack(40)), None));

        assert_eq!(player.get_player().unwrap().weapon_attack(), 40);
    }

    /// Elemental ammo governs the shot: a fire arrow burns whatever bow launches it.
    #[test]
    fn a_bow_fires_with_the_ammos_element() {
        let player = Agent::from_player(armed(
            Some(a_bow(None)),
            Some(a_quiver_holding(an_arrow_of(CombatElement::Fire))),
        ));

        assert_eq!(
            player.get_player().unwrap().weapon_element(),
            CombatElement::Fire
        );
    }

    /// The other half of the `or_else`: an enchanted bow still colours a plain arrow.
    #[test]
    fn an_elemental_bow_keeps_its_element_when_the_ammo_has_none() {
        let player = Agent::from_player(armed(
            Some(a_bow_of(CombatElement::Energy)),
            Some(a_quiver_of_arrows()),
        ));

        assert_eq!(
            player.get_player().unwrap().weapon_element(),
            CombatElement::Energy
        );
    }

    /// Why the element has to reach the plan at all: only `Physical` is blockable, so an
    /// elemental shot must walk past both the shield and the armour of a defended target.
    #[test]
    fn an_elemental_shot_is_not_blocked() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(
                Some(a_bow(None)),
                Some(a_quiver_holding(an_arrow_of(CombatElement::Fire))),
            )),
            a_test_creature_with_defences("Elf", 100, (0, 15), 6, 6),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        let damage = planned_damage(&plan).expect("the shot landed");
        assert_eq!(damage.element, CombatElement::Fire);
        assert!(damage.value > 0);
        assert!(!damage.blocked_shield);
        assert!(!damage.blocked_armor);
    }

    /// A ranged duel across open ground, with every tile around the target landable so a
    /// scattered shot has somewhere to go.
    fn ranged_duel(gap: u16) -> (GameMap, AgentKey, AgentKey, Position) {
        let a = Position::new(10, 10, 7);
        let b = Position::new(10 + gap, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        for dx in -1..=1 {
            for dy in -1..=1 {
                let pos = b.checked_offset(dx, dy).unwrap();
                map.insert_tile(pos.clone(), MapTile::new());
                map.place_item(&pos, None, None, a_ground_item()).unwrap();
            }
        }
        let attacker = map
            .insert_agent(
                Agent::from_player(armed(
                    Some(a_bow(Some(7))),
                    Some(a_quiver_holding(an_arrow_with_hit_chance(-1))),
                )),
                &a,
            )
            .unwrap();
        let target = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &b)
            .unwrap();
        map.get_agent_mut(attacker)
            .unwrap()
            .set_target(Some(target), 0);
        (map, attacker, target, b)
    }

    #[test]
    fn a_shot_that_cannot_hit_plans_no_damage() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(
                Some(a_bow(None)),
                Some(a_quiver_holding(an_arrow_with_hit_chance(-1))),
            )),
            a_test_creature("Rat", 100, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert!(plan.damage.is_empty(), "{plan:?}");
    }

    /// A miss is not a zero: neither block flag is set, and nothing downstream can mistake it
    /// for a hit the shield swallowed.
    #[test]
    fn a_shot_that_cannot_miss_plans_damage() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(
                Some(a_bow(None)),
                Some(a_quiver_holding(an_arrow_with_hit_chance(100))),
            )),
            a_test_creature("Rat", 100, (1, 2)),
        );
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        let damage = planned_damage(&plan).expect("a certain shot lands");
        assert!(damage.value > 0);
        assert!(!damage.blocked_shield && !damage.blocked_armor);
    }

    /// The reference spends the ammunition in `onUsedWeapon`, which runs on both branches of
    /// the hit roll -- and trains nothing on the miss, the same rule that already denies a
    /// fully blocked hit its tick.
    #[test]
    fn a_missed_shot_spends_its_arrow_and_trains_nothing() {
        let mut snapshot = armed(
            Some(a_bow_with_missile(MissileId(3))),
            Some(a_quiver_holding(an_arrow_with_hit_chance(-1))),
        );
        snapshot.skills.insert(
            SkillType::Distance,
            SkillValue {
                value: 20,
                current_ticks: 0,
            },
        );
        let (mut map, attacker, _) = duel(
            Agent::from_player(snapshot),
            a_test_creature("Rat", 100, (1, 2)),
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(0);
        let plan = plan_auto_attack(&map, attacker, &mut h.roll, Tick(0)).unwrap();

        execute_attack(&mut h.ctx(&mut map), plan);

        assert_eq!(broadcast_kinds(&h.events), ["ammo", "missile", "miss"]);
        let arrows = map
            .get_agent(attacker)
            .unwrap()
            .get_player()
            .unwrap()
            .inventory()
            .get(&InventorySlot::RightHand)
            .unwrap()
            .content
            .as_ref()
            .unwrap()[0]
            .amount;
        assert_eq!(arrows, 9, "a miss still costs an arrow");
    }

    /// The missile has to fly to where it landed, not to where it was aimed -- the client
    /// draws the flight from the missile's `to`, and the puff is addressed by that same tile.
    #[test]
    fn a_missed_shot_scatters_onto_a_tile_beside_its_target() {
        let (map, attacker, _, target_pos) = ranged_duel(4);

        let landings: Vec<Position> = (0..40)
            .map(|seed| {
                let mut roll = Rolls::new(seed);
                missile_landing(&plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap())
            })
            .collect();

        assert!(
            landings.iter().all(|pos| pos.is_within(&target_pos, 1)),
            "a shot landed outside the nine tiles: {landings:?}"
        );
        assert!(
            landings.iter().any(|pos| *pos != target_pos),
            "nothing ever scattered off the target's own tile"
        );
    }

    /// The reference does not scatter a point-blank miss, so the puff stays on the target.
    #[test]
    fn a_point_blank_miss_puffs_on_the_targets_own_tile() {
        let (map, attacker, _, target_pos) = ranged_duel(1);

        for seed in 0..40 {
            let mut roll = Rolls::new(seed);
            let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();
            assert!(plan.damage.is_empty());
            assert_eq!(
                missile_landing(&plan),
                target_pos,
                "seed {seed} scattered a point-blank miss"
            );
        }
    }

    /// The reference's `TILESTATE_IMMOVABLEBLOCKSOLID`: a wall turns the tile away, but a
    /// crate someone could push aside does not. Untestable through the planner, because a
    /// wall beside the target also blocks the line of sight the shot needed to be planned.
    #[test]
    fn only_a_wall_and_a_hole_turn_a_missile_away() {
        let mut map = GameMap::new();
        let nothing = Position::new(1, 1, 7);
        let bare = Position::new(2, 1, 7);
        let open = Position::new(3, 1, 7);
        let crate_ = Position::new(4, 1, 7);
        let wall = Position::new(5, 1, 7);
        for pos in [&bare, &open, &crate_, &wall] {
            map.insert_tile(pos.clone(), MapTile::new());
        }
        map.place_item(&open, None, None, a_ground_item()).unwrap();
        map.place_item(&crate_, None, None, a_ground_item())
            .unwrap();
        map.place_item(
            &crate_,
            None,
            None,
            Item::new(
                a_config(ItemId(17), HashSet::from([ItemFlag::Unpass]), Vec::new()),
                1,
            ),
        )
        .unwrap();
        map.place_item(&wall, None, None, a_wall_item()).unwrap();

        assert!(!can_land_missile(&map, &nothing), "no tile at all");
        assert!(!can_land_missile(&map, &bare), "a tile with no ground");
        assert!(can_land_missile(&map, &open));
        assert!(can_land_missile(&map, &crate_), "movable, so not solid");
        assert!(!can_land_missile(&map, &wall));
    }

    /// The other fallback: nothing around the target is landable at all, because none of
    /// those tiles has ground. `category_roll` over an empty slice is `None`, and the shot
    /// has to end up somewhere.
    #[test]
    fn a_miss_over_groundless_tiles_falls_back_to_the_target() {
        let a = Position::new(10, 10, 7);
        let b = Position::new(14, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());
        let attacker = map
            .insert_agent(
                Agent::from_player(armed(
                    Some(a_bow(Some(7))),
                    Some(a_quiver_holding(an_arrow_with_hit_chance(-1))),
                )),
                &a,
            )
            .unwrap();
        let target = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &b)
            .unwrap();
        map.get_agent_mut(attacker)
            .unwrap()
            .set_target(Some(target), 0);
        let mut roll = Rolls::new(1);

        let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();

        assert_eq!(missile_landing(&plan), b);
    }

    #[test]
    fn a_melee_swing_never_misses() {
        let (map, attacker, _) = duel(
            Agent::from_player(armed(Some(a_weapon_with_attack(20)), None)),
            a_test_creature("Rat", 100, (1, 2)),
        );

        for seed in 0..100 {
            let mut roll = Rolls::new(seed);
            let plan = plan_auto_attack(&map, attacker, &mut roll, Tick(0)).unwrap();
            assert!(!plan.damage.is_empty(), "seed {seed} missed with a sword");
        }
    }

    /// A bow's `hit_chance` is a bonus on top of what the arrow rolled, not a chance of its
    /// own: the catalogue's bows carry 1..7 and its arrows carry 87..100.
    #[test]
    fn a_bows_hit_chance_is_a_bonus_on_its_ammos() {
        let plain = Agent::from_player(armed(
            Some(a_bow(Some(5))),
            Some(a_quiver_holding(an_arrow(None))),
        ));
        let enchanted = Agent::from_player(armed(
            Some(a_bow_with_hit_chance(5)),
            Some(a_quiver_holding(an_arrow(None))),
        ));

        let plain = distance_hit_chance(plain.get_player().unwrap(), 1);
        let enchanted = distance_hit_chance(enchanted.get_player().unwrap(), 1);

        assert_eq!(enchanted, plain + 5);
    }

    /// `devileye` carries -20, and a chance below 1 can never meet a 1..=100 roll.
    #[test]
    fn a_negative_hit_chance_can_never_land() {
        let shooter = Agent::from_player(armed(
            Some(a_bow(Some(5))),
            Some(a_quiver_holding(an_arrow_with_hit_chance(-20))),
        ));

        assert!(distance_hit_chance(shooter.get_player().unwrap(), 1) < 1);
    }

    /// A thrown weapon is its own ammunition, so the chance comes off the weapon itself.
    #[test]
    fn a_thrown_weapon_reads_its_own_ceiling() {
        let spear = Item::new(
            a_config(
                ItemId(16),
                HashSet::new(),
                vec![
                    ItemAttribute::WeaponType(WeaponType::Distance),
                    ItemAttribute::WeaponAttack(25),
                    ItemAttribute::WeaponRange(3),
                    ItemAttribute::MaxHitChance(76),
                ],
            ),
            1,
        );
        let thrower = Agent::from_player(armed(Some(spear), None));

        assert_eq!(distance_hit_chance(thrower.get_player().unwrap(), 3), 76);
    }

    /// Only 75, 90 and 100 have a table. Every other ceiling -- 76, 80, 87, 91, 94, 96, which
    /// is most of the catalogue -- **is** the chance, and neither skill nor range moves it.
    #[test]
    fn an_untabled_ceiling_is_itself_the_chance() {
        for skill in [10, 50, 120] {
            for distance in 1..=7 {
                assert_eq!(tabled_hit_chance(91, skill, distance), 91);
                assert_eq!(tabled_hit_chance(76, skill, distance), 76);
            }
        }
    }

    /// Transcribed from `WeaponDistance::useWeapon`. Each entry caps the skill before scaling
    /// it, which is what stops a tabled ceiling being exceeded by skill alone.
    #[test]
    fn the_tabled_ceilings_match_the_reference() {
        assert_eq!(tabled_hit_chance(75, 10, 1), 11);
        assert_eq!(tabled_hit_chance(75, 28, 2), 75, "min(28, 28) * 2.40 + 8");
        assert_eq!(tabled_hit_chance(75, 200, 2), 75, "the cap, not the skill");
        assert_eq!(tabled_hit_chance(90, 10, 1), 13);
        assert_eq!(tabled_hit_chance(90, 45, 3), 90, "min(45, 45) * 2");
        assert_eq!(tabled_hit_chance(90, 200, 3), 90);
        assert_eq!(tabled_hit_chance(100, 30, 2), 100, "min(30, 30) * 3.20 + 4");
        assert_eq!(tabled_hit_chance(100, 87, 6), 100, "min(87, 87) * 1.20 - 4");
    }

    /// Skill is what a tabled ceiling buys, and the reference's caps are what stop it there.
    #[test]
    fn a_tabled_chance_climbs_with_the_distance_skill() {
        let low = tabled_hit_chance(90, 10, 1);
        let high = tabled_hit_chance(90, 74, 1);

        assert!(high > low, "{high} !> {low}");
        assert_eq!(high, tabled_hit_chance(90, 300, 1), "capped at 74");
    }
}
