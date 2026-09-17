use thiserror::Error;
use tracing::{error, warn};

use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::AgentKey,
        items::{Item, ItemAction, ItemFlag, ItemId, ItemRef},
        position::ItemPlacement,
    },
    game::{
        Mark, Tick, TickCtx, TickDelta,
        config::GAME_CONFIG,
        item_movement::{ItemMovementError, insert_item_at, remove_item_at},
    },
};

use super::{events::BroadcastMessage, map_query::find_item};
use crate::persistence::items::ITEM_CONFIGS;

#[derive(Error, Debug)]
pub enum ItemActionError {
    #[error("This item can't be used")]
    ActionFailed,
    #[error("Invalid State")]
    InvalidState,
    #[error("No target")]
    NoTarget,
    #[error("You're already full")]
    PlayerFull,
}

pub fn decay_item(ctx: &mut TickCtx, item_ref: ItemRef) {
    let mark = ctx.mark();
    let Some(item) = find_item(ctx.map, &item_ref.placement, &item_ref.guid) else {
        return;
    };
    let Some((_, decay_to)) = item.config.attr_decay() else {
        return;
    };
    let Some(config) = ITEM_CONFIGS.get(&decay_to) else {
        if decay_to != ItemId(0) {
            error!("Config not found for item id {decay_to}");
        }
        return;
    };

    let new_item = if let Some(fluid) = item.fluid {
        Item::new_fluid(config.clone(), fluid)
    } else {
        Item::new(config.clone(), 1)
    };
    check_decay(
        ctx.scheduled,
        &new_item,
        item_ref.placement.clone(),
        ctx.tick,
    );
    let Ok((old_item, source_index)) = remove_item_at(ctx, &item_ref, 1) else {
        ctx.rollback_to(mark);
        return;
    };
    if insert_item_at(ctx, new_item, &item_ref.placement, source_index).is_err() {
        if let Err(e) = insert_item_at(ctx, old_item.clone(), &item_ref.placement, source_index) {
            error!(
                "Failed to revert item move. Item {:?} at {:?}. Error {}",
                old_item, item_ref.placement, e
            );
        }
        ctx.rollback_to(mark);
    }
}

/// Takes the raw command accumulator rather than a [`TickCtx`]: `damage::draw_blood` calls it
/// with an `&Item` still borrowed out of the map, so the whole context cannot be lent here.
pub fn check_decay(
    commands: &mut Vec<ScheduledCommand>,
    item: &Item,
    placement: ItemPlacement,
    current_tick: Tick,
) {
    if let Some((duration, _)) = item.config.attr_decay() {
        commands.push(ScheduledCommand {
            at_tick: current_tick + duration,
            command: WorldCommand::DecayItem {
                item: ItemRef {
                    guid: item.guid.clone(),
                    placement,
                },
            },
        });
    }
}

pub fn use_item(ctx: &mut TickCtx, agent_key: AgentKey, item_ref: ItemRef) {
    let mark = ctx.mark();
    if ctx
        .map
        .get_agent(agent_key)
        .map(|agent| agent.next_use_tick > ctx.tick)
        .unwrap_or(false)
    {
        return use_item_failed(ctx, mark, agent_key, "Can't use that fast");
    }

    if ctx
        .map
        .agent_position(agent_key)
        .filter(|player_pos| player_pos.placement_is_adjacent(&item_ref.placement))
        .is_none()
    {
        return use_item_failed(ctx, mark, agent_key, "Item is too far");
    }

    let Some(item) = find_item(ctx.map, &item_ref.placement, &item_ref.guid) else {
        return use_item_failed(ctx, mark, agent_key, "Item was not found");
    };

    if !item.config.has_flag(ItemFlag::Usable) {
        return use_item_failed(ctx, mark, agent_key, "Can't use that");
    }

    let is_container = item.config.has_flag(ItemFlag::Container);
    let action = item.config.attr_action();

    if is_container {
        ctx.events.push(BroadcastMessage::OpenContainer {
            agent_key,
            item: item_ref,
        });
        return;
    } else if let Some(action) = action {
        match route_action(ctx, &action, agent_key, &item_ref) {
            Ok(()) => {
                ctx.map.get_agent_mut(agent_key).unwrap().next_use_tick =
                    ctx.tick + GAME_CONFIG.action.use_item_cooldown_ticks;
                return;
            }
            Err(e) => {
                let message = match e {
                    ItemActionError::InvalidState => {
                        warn!("{e}");
                        "Item cannot be used"
                    }
                    e => &e.to_string(),
                };
                use_item_failed(ctx, mark, agent_key, message);
            }
        }
    }
}

/// Discards whatever a half-finished use reported and announces the refusal in its place.
fn use_item_failed(ctx: &mut TickCtx, mark: Mark, agent_key: AgentKey, message: &str) {
    ctx.rollback_to(mark);
    ctx.events.push(BroadcastMessage::UseItemDenied {
        agent_key,
        message: message.to_owned(),
    });
}

pub fn route_action(
    ctx: &mut TickCtx,
    action: &ItemAction,
    agent_key: AgentKey,
    item: &ItemRef,
) -> Result<(), ItemActionError> {
    match action {
        ItemAction::Transform { into } => transform(ctx, item, *into),
        ItemAction::Door { new } => toggle_door(ctx, item, *new),
        ItemAction::Food {
            duration,
            message_index,
        } => eat_food(ctx, item, agent_key, *duration, *message_index),
    }
}

pub(super) fn transform(
    ctx: &mut TickCtx,
    item: &ItemRef,
    into: ItemId,
) -> Result<(), ItemActionError> {
    let Some(config) = ITEM_CONFIGS.get(&into) else {
        error!(
            "cannot transform {:?} into {into}: no such item config",
            item.guid
        );
        return Err(ItemActionError::ActionFailed);
    };

    let Ok((old_item, source_index)) = remove_item_at(ctx, item, 1) else {
        return Err(ItemActionError::ActionFailed);
    };

    let new_item = Item::new(config.clone(), 1);
    check_decay(ctx.scheduled, &new_item, item.placement.clone(), ctx.tick);

    if let Err(e) = insert_item_at(ctx, new_item.clone(), &item.placement, source_index) {
        let result = match e {
            ItemMovementError::NotEnoughCap
                if let ItemPlacement::Inventory(_, agent_key) = &item.placement =>
            {
                if let Some(pos) = ctx.map.agent_position(*agent_key).cloned() {
                    insert_item_at(ctx, new_item, &ItemPlacement::Map(pos), None)
                } else {
                    Err(ItemMovementError::PlayerDespawned)
                }
            }
            e => Err(e),
        };

        if result.is_err() {
            let guid = old_item.guid.clone();
            if let Err(e) = insert_item_at(ctx, old_item, &item.placement, source_index) {
                error!(
                    "Failed to revert item move. Item {:?} at {:?}. Error: {}",
                    guid, item.placement, e
                );
            }

            return Err(ItemActionError::ActionFailed);
        }
    }

    Ok(())
}

fn toggle_door(ctx: &mut TickCtx, item: &ItemRef, new_door: ItemId) -> Result<(), ItemActionError> {
    let Some(config) = ITEM_CONFIGS.get(&new_door) else {
        error!(
            "cannot transform {:?} into {new_door}: no such item config",
            item.guid
        );
        return Err(ItemActionError::ActionFailed);
    };

    let ItemPlacement::Map(pos) = &item.placement else {
        return Err(ItemActionError::InvalidState);
    };

    let Ok((old_item, source_index)) = remove_item_at(ctx, item, 1) else {
        return Err(ItemActionError::ActionFailed);
    };

    let new_door = Item::new(config.clone(), 1);
    if insert_item_at(ctx, new_door, &item.placement, source_index).is_err() {
        insert_item_at(ctx, old_item, &item.placement, source_index)
            .map_err(|_| ItemActionError::InvalidState)?;
    }

    for _agent_key in ctx
        .map
        .iter_agents_at(pos)
        .map_err(|_| ItemActionError::InvalidState)?
    {
        // todo: move agents from closed door.
    }

    Ok(())
}

fn eat_food(
    ctx: &mut TickCtx,
    item: &ItemRef,
    agent_key: AgentKey,
    duration: TickDelta,
    message_index: usize,
) -> Result<(), ItemActionError> {
    let agent = ctx
        .map
        .get_agent(agent_key)
        .ok_or(ItemActionError::InvalidState)?;
    if !agent.conditions().can_feed(ctx.tick, duration) {
        return Err(ItemActionError::PlayerFull);
    }
    let old_generation = agent
        .conditions()
        .fed()
        .filter(|(until, _)| *until > ctx.tick)
        .map(|(_, generation)| generation);
    let position = ctx
        .map
        .agent_position(agent_key)
        .ok_or(ItemActionError::InvalidState)?
        .clone();

    if remove_item_at(ctx, item, 1).is_err() {
        return Err(ItemActionError::InvalidState);
    }

    let agent = ctx.map.get_agent_mut(agent_key).unwrap();
    let generation = agent.conditions_mut().add_fed_ticks(ctx.tick, duration);

    if Some(generation) != old_generation {
        ctx.scheduled.push(ScheduledCommand {
            at_tick: ctx.tick + GAME_CONFIG.regen_ticks,
            command: WorldCommand::RegeneratePlayer {
                agent_key,
                generation,
            },
        });
    }

    if let Some(message) = GAME_CONFIG.action_messages.get(message_index) {
        ctx.events.push(BroadcastMessage::AgentActionMessage {
            position,
            message: message.clone(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::items::{ItemConfig, ItemId};
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::position::Position;
    use crate::game::TestHarness;

    /// `into` comes from data -- a `transform(N)` attribute or a diggable's `id + 1` -- so
    /// an id the catalogue does not carry is reachable by editing an asset file.
    #[test]
    fn a_transform_into_an_unknown_item_refuses_and_keeps_the_original() {
        let missing = ItemId(65535);
        assert!(
            !ITEM_CONFIGS.contains_key(&missing),
            "{missing:?} must stay absent for this test to mean anything"
        );

        let pos = Position::new(10, 10, 7);
        let sand = Item::new(
            std::sync::Arc::new(ItemConfig::new(
                ItemId(4322),
                "sand".to_string(),
                None,
                None,
                [ItemFlag::Ground],
                Vec::new(),
            )),
            1,
        );
        let guid = sand.guid.clone();
        let mut tile = MapTile::new();
        tile.push_item(sand);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), tile);

        let mut h = TestHarness::new();
        let result = transform(
            &mut h.ctx(&mut map),
            &ItemRef {
                guid: guid.clone(),
                placement: ItemPlacement::Map(pos.clone()),
            },
            missing,
        );

        assert!(result.is_err());
        assert!(
            map.get_item_by_id(&pos, &guid).is_some(),
            "the original item was destroyed"
        );
        assert!(
            h.events.is_empty(),
            "a refused transform must not report a change: {:?}",
            h.events
        );
        assert!(h.scheduled.is_empty());
    }
}
