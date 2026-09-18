use crate::{
    entities::{
        agent::AgentKey,
        effects::AreaEffect,
        healing::{HealPlan, Restore, RestoreType},
        map::GameMap,
        spells::SpellHealing,
        targeting::{AreaTarget, TargetFilter, TargetMode},
    },
    game::{
        TickCtx,
        config::GAME_CONFIG,
        events::BroadcastMessage,
        random::Rolls,
        spells::{SpellCastingDenyReason, resolve_targets, roll_power},
    },
};

pub fn plan_healing_spell(
    map: &GameMap,
    caster: AgentKey,
    roll: &mut Rolls,
    spell: &SpellHealing,
    area_target: &AreaTarget,
    param: Option<&str>,
) -> Result<HealPlan, SpellCastingDenyReason> {
    let player = map
        .get_player(caster)
        .ok_or(SpellCastingDenyReason::InvalidState(
            caster,
            "non player casting spell",
        ))?;

    let filter = match spell.target {
        TargetMode::Aimed => TargetFilter::Any,
        _ => TargetFilter::Players,
    };

    let targets = resolve_targets(map, caster, &spell.target, area_target, param, filter)?;

    let area_effect = targets
        .delta
        .zip(targets.aim)
        .map(|(delta, origin)| AreaEffect {
            effect_id: GAME_CONFIG.effect_ids.healing_spell,
            origin,
            delta,
        });

    let life = roll_power(player, &spell.power, roll, false);

    Ok(HealPlan {
        caster,
        restores: targets
            .keys
            .into_iter()
            .map(|target| {
                (
                    target,
                    Restore {
                        life: Some(life),
                        mana: None,
                    },
                )
            })
            .collect(),
        area_effect,
    })
}

pub fn execute_healing(ctx: &mut TickCtx, plan: HealPlan) {
    if let Some(area) = plan.area_effect {
        ctx.events
            .push(BroadcastMessage::AreaEffectAppeared { area_effect: area });
    }

    for (agent_key, restore) in &plan.restores {
        restore_agent(ctx, *agent_key, restore);
    }

    if plan
        .restores
        .iter()
        .find(|(key, _)| *key == plan.caster)
        .is_none()
    {
        let Some(caster_pos) = ctx.map.agent_position(plan.caster) else {
            return;
        };
        ctx.events.push(BroadcastMessage::AreaEffectAppeared {
            area_effect: AreaEffect::single(
                GAME_CONFIG.effect_ids.healing_spell,
                caster_pos.clone(),
            ),
        });
    }
}

fn restore_agent(ctx: &mut TickCtx, agent_key: AgentKey, restore: &Restore) {
    let Some(pos) = ctx.map.agent_position(agent_key).cloned() else {
        return;
    };
    let Some(agent) = ctx.map.get_agent_mut(agent_key) else {
        return;
    };

    let life = restore
        .life
        .map(|amount| amount.min(agent.life().missing()));
    let mana = restore
        .mana
        .map(|amount| amount.min(agent.mana().missing()));

    if let Some(life) = life {
        agent.restore_life(life);
        ctx.events.push(BroadcastMessage::AgentHealed {
            agent_key,
            position: pos.clone(),
            amount: life,
            restore_type: RestoreType::Life,
        });
    }
    if let Some(mana) = mana {
        agent.restore_mana(mana);
        ctx.events.push(BroadcastMessage::AgentHealed {
            agent_key,
            position: pos.clone(),
            amount: mana,
            restore_type: RestoreType::Mana,
        });
    }
}
