use smallvec::SmallVec;

use crate::{
    entities::{
        Bounds,
        agent::AgentKey,
        combat::{AttackCost, AttackPlan, CombatDamage},
        creature::{AbilityEffect, CreatureAbilityId, CreatureAttack},
        effects::{AreaEffect, Missile},
        healing::{HealPlan, Restore},
        map::GameMap,
        targeting::{AreaTarget, TargetFilter},
    },
    game::{
        TickCtx, combat::execute_attack, healing::execute_healing, random::Rolls,
        spells::resolve_targets,
    },
};

pub fn cast_ability(ctx: &mut TickCtx, agent_key: AgentKey, ability_id: CreatureAbilityId) {
    let Some(agent) = ctx.map.get_agent(agent_key) else {
        return;
    };
    let Some(kind) = agent.get_creature_kind() else {
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
    }

    if let Some(agent) = ctx.map.get_agent_mut(agent_key) {
        agent.stamp_spell_group(ctx.tick, group, None);
    }
}

fn plan_ability_attack(
    map: &GameMap,
    creature: AgentKey,
    attack: &CreatureAttack,
    roll: &mut Rolls,
) -> Option<AttackPlan> {
    let target = map.get_agent(creature)?.target()?;
    let from = map.agent_position(creature)?.clone();

    let mut targets = resolve_targets(
        map,
        creature,
        &attack.target,
        &AreaTarget::Agent(target),
        None,
        TargetFilter::Players,
    )
    .ok()?;
    targets.keys.retain(|key| creature != *key);

    let missile = attack
        .missile_id
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
            .zip(attack.effect_id)
            .map(|((delta, origin), effect_id)| AreaEffect {
                effect_id,
                origin,
                delta,
            });

    let value = roll.uniform(attack.damage.value.min, attack.damage.value.max);
    let element = attack.damage.element;
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

    Some(AttackPlan {
        attacker: creature,
        damage,
        cost: AttackCost::None,
        trains: None,
        missile,
        area_effect,
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
        let (mut map, demon) = a_map_with(AbilityEffect::Attack(CreatureAttack {
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
        let (mut map, demon) = a_map_with(AbilityEffect::Heal(Bounds { min: 5, max: 5 }));
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);

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
}
