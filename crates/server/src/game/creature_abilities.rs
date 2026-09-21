use smallvec::SmallVec;

use crate::{
    entities::{
        Bounds,
        agent::AgentKey,
        combat::{AttackCost, AttackPlan, CombatDamage},
        creature::{AbilityEffect, CreatureAbilityId, CreatureAttack},
        effects::{AreaEffect, EffectId, Missile, MissileId},
        healing::{HealPlan, Restore},
        map::GameMap,
        targeting::{AreaTarget, TargetFilter, TargetMode},
    },
    game::{
        TickCtx, combat::execute_attack, conditions::apply_condition, events::BroadcastMessage,
        healing::execute_healing, random::Rolls, spells::resolve_targets, support::cast_support,
    },
};

pub fn cast_ability(ctx: &mut TickCtx, agent_key: AgentKey, ability_id: CreatureAbilityId) {
    let Some(kind) = ctx
        .map
        .get_agent(agent_key)
        .and_then(|agent| agent.creature_kind())
        .cloned()
    else {
        return;
    };
    let Some(effect) = kind.get_ability_effect(ability_id) else {
        return;
    };
    let group = effect.cooldown_group();

    match effect {
        AbilityEffect::Attack(attack) => {
            if let Some(plan) = plan_ability_attack(ctx.map, agent_key, attack, ctx.roll) {
                execute_attack(ctx, plan);
            }
        }
        AbilityEffect::Heal(life) => {
            let plan = plan_creature_heal(agent_key, life, ctx.roll);
            execute_healing(ctx, plan);
        }
        AbilityEffect::Support(cast) => {
            let aimed_at = ctx
                .map
                .get_agent(agent_key)
                .and_then(|agent| agent.target())
                .map_or(AreaTarget::None, AreaTarget::Agent);
            if let Ok(targets) = resolve_targets(
                ctx.map,
                agent_key,
                &cast.target,
                &aimed_at,
                None,
                TargetFilter::Players,
            ) {
                cast_support(ctx, agent_key, cast, targets);
            }
        }
        AbilityEffect::Condition(attack) => {
            if let Some(strike) = resolve_strike(
                ctx.map,
                agent_key,
                &attack.target,
                attack.effect_id,
                attack.missile_id,
            ) {
                if let Some(missile) = strike.missile {
                    ctx.events
                        .push(BroadcastMessage::MissileLaunched { missile });
                }
                if let Some(area_effect) = strike.area_effect {
                    ctx.events
                        .push(BroadcastMessage::AreaEffectAppeared { area_effect });
                }
                for target in strike.targets {
                    apply_condition(ctx, target, &attack.condition, Some(agent_key));
                }
            }
        }
    }

    if let Some(mut agent) = ctx.map.agent_mut(agent_key) {
        agent.stamp_spell_group(ctx.tick, group, None);
    }
}

struct AbilityStrike {
    targets: Vec<AgentKey>,
    missile: Option<Missile>,
    area_effect: Option<AreaEffect>,
}

fn resolve_strike(
    map: &GameMap,
    creature: AgentKey,
    target: &TargetMode,
    effect_id: Option<EffectId>,
    missile_id: Option<MissileId>,
) -> Option<AbilityStrike> {
    let aimed_at = map.get_agent(creature)?.target()?;
    let from = map.agent_position(creature)?.clone();

    let mut targets = resolve_targets(
        map,
        creature,
        target,
        &AreaTarget::Agent(aimed_at),
        None,
        TargetFilter::Players,
    )
    .ok()?;
    targets.keys.retain(|key| creature != *key);

    let missile = missile_id
        .zip(targets.aim.clone())
        .map(|(missile_id, to)| Missile {
            missile_id,
            from,
            to,
        });
    let area_effect =
        targets
            .delta
            .zip(targets.aim)
            .zip(effect_id)
            .map(|((delta, origin), effect_id)| AreaEffect {
                effect_id,
                origin,
                delta,
            });

    Some(AbilityStrike {
        targets: targets.keys,
        missile,
        area_effect,
    })
}

fn plan_ability_attack(
    map: &GameMap,
    creature: AgentKey,
    attack: &CreatureAttack,
    roll: &mut Rolls,
) -> Option<AttackPlan> {
    let strike = resolve_strike(
        map,
        creature,
        &attack.target,
        attack.effect_id,
        attack.missile_id,
    )?;

    let value = roll.uniform(attack.damage.value.min, attack.damage.value.max);
    let element = attack.damage.element;
    let damage = strike
        .targets
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

    Some(AttackPlan {
        attacker: creature,
        damage,
        cost: AttackCost::None,
        trains: None,
        missile: strike.missile,
        area_effect: strike.area_effect,
        missed: false,
        condition: attack.damage.condition.clone(),
    })
}

fn plan_creature_heal(creature: AgentKey, bounds: &Bounds, roll: &mut Rolls) -> HealPlan {
    HealPlan {
        caster: creature,
        restores: SmallVec::from([(
            creature,
            Restore {
                life: Some(roll.uniform(bounds.min, bounds.max)),
                mana: None,
            },
        )]),
        area_effect: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::items::MAX_DROP_CHANCE;
    use crate::entities::Bounds;
    use crate::entities::agent::Agent;
    use crate::entities::combat::CombatElement;
    use crate::entities::creature::{CreatureAbility, CreatureAttackDamage, CreatureKind};
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::position::Position;
    use crate::entities::spells::SpellGroup;
    use crate::entities::targeting::TargetMode;
    use crate::entities::world_map::WorldMap;
    use crate::game::config::GAME_CONFIG;
    use crate::game::{TestHarness, Tick, TickDelta};
    use crate::persistence::test_fixtures::{a_creature_kind, a_test_snapshot};
    use std::sync::Arc;

    fn at(x: u16) -> Position {
        Position::new(x, 10, 7)
    }

    fn a_map_with(effect: AbilityEffect) -> (GameMap, AgentKey) {
        let mut map = GameMap::new();
        for x in 14..=17 {
            map.insert_tile(at(x), MapTile::new());
        }
        let demon = map
            .insert_agent(
                Agent::from_creature_kind(
                    Arc::new(CreatureKind {
                        abilities: vec![CreatureAbility {
                            id: CreatureAbilityId(0),
                            cooldown: TickDelta(40),
                            chance: MAX_DROP_CHANCE,
                            effect,
                        }],
                        ..a_creature_kind("Demon")
                    }),
                    at(15),
                ),
                &at(15),
            )
            .unwrap();
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(16))
            .unwrap();
        map.get_agent_mut(demon)
            .unwrap()
            .set_target(Some(player), 1);
        (map, demon)
    }

    #[test]
    fn an_attack_ability_stamps_the_attack_group() {
        let (map, demon) = a_map_with(AbilityEffect::Attack(CreatureAttack {
            damage: CreatureAttackDamage {
                element: CombatElement::Energy,
                value: Bounds { min: 1, max: 2 },
                condition: None,
            },
            target: TargetMode::Target { range: 1 },
            effect_id: None,
            missile_id: None,
        }));
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);

        let mut map = WorldMap::new(map);
        cast_ability(&mut h.ctx(&mut map), demon, CreatureAbilityId(0));

        assert_eq!(
            map.get_agent(demon)
                .unwrap()
                .next_spell_group_tick(SpellGroup::Attack),
            Tick(100) + GAME_CONFIG.combat.attack_group_cooldown
        );
    }

    #[test]
    fn a_heal_ability_stamps_the_healing_group() {
        let (map, demon) = a_map_with(AbilityEffect::Heal(Bounds { min: 5, max: 5 }));
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);

        let mut map = WorldMap::new(map);
        cast_ability(&mut h.ctx(&mut map), demon, CreatureAbilityId(0));

        let agent = map.get_agent(demon).unwrap();
        assert_eq!(
            agent.next_spell_group_tick(SpellGroup::Healing),
            Tick(100) + GAME_CONFIG.combat.healing_group_cooldown
        );
        assert_eq!(
            agent.next_spell_group_tick(SpellGroup::Attack),
            Tick(0),
            "a heal must not throttle the attack group"
        );
    }

    use crate::entities::conditions::SpeedEffect;
    use crate::entities::support::{SpeedFormula, SpeedTerm, SupportCast, SupportEffect};

    fn a_speed_cast(target: TargetMode, effect: SpeedEffect, change: i16) -> AbilityEffect {
        let term = SpeedTerm {
            factor: 0.0,
            flat: change,
        };
        AbilityEffect::Support(SupportCast {
            target,
            effect: SupportEffect::Speed {
                effect,
                formula: SpeedFormula {
                    min: term.clone(),
                    max: term,
                },
                duration: TickDelta(100),
            },
            effect_id: None,
            missile_id: None,
        })
    }

    #[test]
    fn a_support_ability_on_itself_hastes_the_creature_and_stamps_the_support_group() {
        let (map, demon) = a_map_with(a_speed_cast(TargetMode::Caster, SpeedEffect::Haste, 30));
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);

        let mut map = WorldMap::new(map);
        cast_ability(&mut h.ctx(&mut map), demon, CreatureAbilityId(0));

        let agent = map.get_agent(demon).unwrap();
        assert_eq!(agent.speed(), 130);
        assert_eq!(
            agent.next_spell_group_tick(SpellGroup::Support),
            Tick(100) + GAME_CONFIG.combat.support_group_cooldown
        );
    }

    #[test]
    fn a_paralysing_ability_slows_the_creatures_target() {
        let (map, demon) = a_map_with(a_speed_cast(
            TargetMode::Target { range: 1 },
            SpeedEffect::Paralysis,
            -50,
        ));
        let player = map.get_agent(demon).unwrap().target().unwrap();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        cast_ability(&mut h.ctx(&mut map), demon, CreatureAbilityId(0));

        assert_eq!(map.get_agent(player).unwrap().speed(), 70);
    }

    use crate::actors::world::WorldCommand;
    use crate::entities::conditions::{ConditionSpec, SpecSchedule};
    use crate::entities::creature::ConditionAttack;

    #[test]
    fn a_condition_attack_poisons_its_target_without_a_hit() {
        let (map, demon) = a_map_with(AbilityEffect::Condition(ConditionAttack {
            condition: ConditionSpec {
                element: CombatElement::Earth,
                damage: Bounds { min: 40, max: 40 },
                interval: TickDelta(80),
                schedule: SpecSchedule::Decaying { start: None },
                delayed: true,
            },
            target: TargetMode::Target { range: 1 },
            effect_id: None,
            missile_id: None,
        }));
        let player = map.get_agent(demon).unwrap().target().unwrap();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        cast_ability(&mut h.ctx(&mut map), demon, CreatureAbilityId(0));

        assert_eq!(map.get_agent(player).unwrap().life().current, 100);
        assert!(
            !h.events
                .iter()
                .any(|e| matches!(e, BroadcastMessage::DamageTaken { .. }))
        );
        assert!(h.scheduled.iter().any(|s| matches!(
            s.command,
            WorldCommand::DamageOverTimeTick { agent_key, .. } if agent_key == player
        )));
    }
}
