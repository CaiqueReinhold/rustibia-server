use std::sync::Arc;

use thiserror::Error;
use tracing::{error, warn};

use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::AgentKey,
        conditions::TimedCondition,
        items::{ClimbDirection, Item, ItemAction, ItemConfig, ItemFlag, ItemId, ItemRef},
        map::GameMap,
        position::{Direction, ItemPlacement, Position},
    },
    game::{
        Mark, Tick, TickCtx, TickDelta, conditions,
        config::GAME_CONFIG,
        item_movement::{ItemMovementError, insert_item_at, remove_item_at},
        movement::{on_step, push},
    },
};

use super::{
    events::BroadcastMessage,
    map_query::{find_item, find_landing},
};
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
    #[error("Already reported")]
    Reported,
    #[error("There is not enough room.")]
    NoRoom,
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

    let new_item = decayed_into(item, config.clone());
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

fn decayed_into(item: &Item, config: Arc<ItemConfig>) -> Item {
    let mut next = match item.fluid {
        Some(fluid) => Item::new_fluid(config, fluid),
        None => Item::new(config, 1),
    };
    next.owner = item.owner;
    next.action_id = item.action_id;
    next
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
                    guid: item.guid,
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
                ctx.map
                    .agent_mut(agent_key)
                    .unwrap()
                    .stamp_use(ctx.tick + GAME_CONFIG.action.use_item_cooldown_ticks);
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
        ItemAction::Door { new } => match ITEM_CONFIGS.get(new) {
            Some(config) => toggle_door(ctx, item, config),
            None => {
                error!(
                    "cannot turn door {:?} into {new}: no such item config",
                    item.guid
                );
                Err(ItemActionError::ActionFailed)
            }
        },
        ItemAction::Food {
            duration,
            message_index,
        } => eat_food(ctx, item, agent_key, *duration, *message_index),
        ItemAction::Climb(direction) => climb(ctx, agent_key, item, *direction),
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

    let mut new_item = Item::new(config.clone(), 1);
    new_item.action_id = old_item.action_id;
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
            let guid = old_item.guid;
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

const PUSH_ORDER: [Direction; 4] = [
    Direction::East,
    Direction::South,
    Direction::North,
    Direction::West,
];

fn toggle_door(
    ctx: &mut TickCtx,
    item: &ItemRef,
    new_door: &Arc<ItemConfig>,
) -> Result<(), ItemActionError> {
    let ItemPlacement::Map(pos) = &item.placement else {
        return Err(ItemActionError::InvalidState);
    };
    let pushes = if new_door.has_flag(ItemFlag::Unpass) {
        plan_door_pushes(ctx.map, pos)?
    } else {
        Vec::new()
    };

    let Ok((old_door, source_index)) = remove_item_at(ctx, item, 1) else {
        return Err(ItemActionError::ActionFailed);
    };
    let mut door = Item::new(new_door.clone(), 1);
    door.action_id = old_door.action_id;
    if insert_item_at(ctx, door, &item.placement, source_index).is_err() {
        let guid = old_door.guid;
        if let Err(e) = insert_item_at(ctx, old_door, &item.placement, source_index) {
            error!("Failed to restore door {guid:?} at {pos}. Error: {e}");
        }
        return Err(ItemActionError::ActionFailed);
    }

    for (agent_key, direction) in pushes {
        push(ctx, agent_key, direction);
    }
    Ok(())
}

/// Where each agent in the doorway goes when the door closes; refuses unless every one of them
/// has a tile and nothing movable lies in the doorway.
fn plan_door_pushes(
    map: &GameMap,
    door: &Position,
) -> Result<Vec<(AgentKey, Direction)>, ItemActionError> {
    if map
        .iter_items(door)
        .map_err(|_| ItemActionError::InvalidState)?
        .any(|item| !item.config.has_flag(ItemFlag::Unmove))
    {
        return Err(ItemActionError::NoRoom);
    }
    let occupants: Vec<AgentKey> = map
        .iter_agents_at(door)
        .map_err(|_| ItemActionError::InvalidState)?
        .copied()
        .collect();

    let mut taken: Vec<Position> = Vec::new();
    let mut pushes = Vec::new();
    for agent_key in occupants {
        let direction = PUSH_ORDER
            .into_iter()
            .find(|direction| {
                let to = door.clone() + *direction;
                !taken.contains(&to)
                    && map.can_move(&to, agent_key)
                    && map.tile_friction(&to).is_some()
            })
            .ok_or(ItemActionError::NoRoom)?;
        taken.push(door.clone() + direction);
        pushes.push((agent_key, direction));
    }
    Ok(pushes)
}

fn climb(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    item: &ItemRef,
    direction: ClimbDirection,
) -> Result<(), ItemActionError> {
    let ItemPlacement::Map(pos) = &item.placement else {
        return Err(ItemActionError::InvalidState);
    };
    let landing = match direction {
        ClimbDirection::Up => pos
            .z
            .checked_sub(1)
            .and_then(|z| find_landing(ctx.map, agent_key, &Position::new(pos.x, pos.y, z), false)),
        ClimbDirection::Down => find_landing(
            ctx.map,
            agent_key,
            &Position::new(pos.x, pos.y, pos.z + 1),
            true,
        ),
    }
    .ok_or(ItemActionError::NoRoom)?;
    let from = ctx
        .map
        .agent_position(agent_key)
        .ok_or(ItemActionError::InvalidState)?
        .clone();

    ctx.map
        .move_agent(agent_key, &landing)
        .map_err(|_| ItemActionError::InvalidState)?;
    ctx.events.push(BroadcastMessage::AgentTeleported {
        agent_key,
        from_position: from,
        to_position: landing.clone(),
    });
    on_step(ctx, agent_key, &landing);
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

    let tick = ctx.tick;
    let generation = ctx
        .map
        .agent_mut(agent_key)
        .unwrap()
        .conditions(|c| c.add_fed_ticks(tick, duration));

    if Some(generation) != old_generation {
        ctx.scheduled.push(ScheduledCommand {
            at_tick: ctx.tick + GAME_CONFIG.regen_ticks,
            command: WorldCommand::RegeneratePlayer {
                agent_key,
                generation,
            },
        });
        conditions::schedule_expiry(ctx, agent_key, TimedCondition::Fed);
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
    use crate::entities::world_map::WorldMap;
    use crate::game::TestHarness;
    use std::sync::Arc;

    fn a_floor() -> MapTile {
        let mut tile = MapTile::new();
        tile.push_item(Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1),
                "ground".to_string(),
                None,
                None,
                [ItemFlag::Ground, ItemFlag::Unmove],
                vec![crate::entities::items::ItemAttribute::TileFriction(100)],
            )),
            1,
        ));
        tile
    }

    fn a_climbable(direction: ClimbDirection) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1948),
                "ladder".to_string(),
                None,
                None,
                [ItemFlag::Usable, ItemFlag::Unmove],
                vec![crate::entities::items::ItemAttribute::Action(
                    ItemAction::Climb(direction),
                )],
            )),
            1,
        )
    }

    /// A player at (10, 11, 7) beside a climbable item at (10, 10, 7), and floors at `floors`.
    fn a_climb(direction: ClimbDirection, floors: &[Position]) -> (WorldMap, AgentKey, ItemRef) {
        let (ladder_pos, player_pos) = (Position::new(10, 10, 7), Position::new(10, 11, 7));
        let mut map = GameMap::new();
        let mut ladder_tile = a_floor();
        let ladder = a_climbable(direction);
        let item = ItemRef {
            guid: ladder.guid,
            placement: ItemPlacement::Map(ladder_pos.clone()),
        };
        ladder_tile.push_item(ladder);
        map.insert_tile(ladder_pos, ladder_tile);
        map.insert_tile(player_pos.clone(), a_floor());
        for pos in floors {
            map.insert_tile(pos.clone(), a_floor());
        }
        let player = map
            .insert_agent(
                crate::entities::agent::Agent::from_player(
                    crate::persistence::test_fixtures::a_test_snapshot(1, 1),
                ),
                &player_pos,
            )
            .unwrap();
        (WorldMap::new(map), player, item)
    }

    fn denial(h: &TestHarness) -> Option<&str> {
        h.events.iter().find_map(|e| match e {
            BroadcastMessage::UseItemDenied { message, .. } => Some(message.as_str()),
            _ => None,
        })
    }

    #[test]
    fn a_ladder_lands_south_of_itself_on_the_floor_above() {
        let landing = Position::new(10, 11, 6);
        let (mut map, player, ladder) = a_climb(
            ClimbDirection::Up,
            &[Position::new(10, 9, 6), landing.clone()],
        );
        let mut h = TestHarness::new();

        use_item(&mut h.ctx(&mut map), player, ladder);

        assert_eq!(denial(&h), None);
        assert_eq!(map.agent_position(player), Some(&landing));
        assert!(
            h.events
                .iter()
                .any(|e| matches!(e, BroadcastMessage::AgentTeleported { .. }))
        );
    }

    #[test]
    fn a_grate_lands_directly_below() {
        let below = Position::new(10, 10, 8);
        let (mut map, player, grate) = a_climb(
            ClimbDirection::Down,
            &[below.clone(), Position::new(10, 11, 8)],
        );
        let mut h = TestHarness::new();

        use_item(&mut h.ctx(&mut map), player, grate);

        assert_eq!(map.agent_position(player), Some(&below));
    }

    #[test]
    fn a_ladder_with_nowhere_to_land_is_refused() {
        let (mut map, player, ladder) = a_climb(ClimbDirection::Up, &[]);
        let mut h = TestHarness::new();

        use_item(&mut h.ctx(&mut map), player, ladder);

        assert_eq!(denial(&h), Some("There is not enough room."));
        assert_eq!(map.agent_position(player), Some(&Position::new(10, 11, 7)));
    }

    fn a_door_config(id: u16, closed: bool) -> Arc<ItemConfig> {
        let mut flags = vec![ItemFlag::Usable, ItemFlag::Unmove];
        if closed {
            flags.push(ItemFlag::Unpass);
        }
        Arc::new(ItemConfig::new(
            ItemId(id),
            "door".to_string(),
            None,
            None,
            flags,
            Vec::new(),
        ))
    }

    const DOOR: Position = Position { x: 10, y: 10, z: 7 };

    /// An open door at `DOOR` with floors around it, minus `blocked`, and the item reference to
    /// it.
    fn a_doorway(blocked: &[Position]) -> (GameMap, ItemRef) {
        let mut map = GameMap::new();
        let mut tile = a_floor();
        let mut door = Item::new(a_door_config(1630, false), 1);
        door.action_id = std::num::NonZeroU16::new(1001);
        let item = ItemRef {
            guid: door.guid,
            placement: ItemPlacement::Map(DOOR),
        };
        tile.push_item(door);
        map.insert_tile(DOOR, tile);
        for (dx, dy) in [(1, 0), (0, 1), (0, -1), (-1, 0)] {
            let pos = Position::new((10 + dx) as u16, (10 + dy) as u16, 7);
            if !blocked.contains(&pos) {
                map.insert_tile(pos, a_floor());
            }
        }
        (map, item)
    }

    fn a_player_at(map: &mut GameMap, pos: &Position, id: u32) -> AgentKey {
        map.insert_agent(
            crate::entities::agent::Agent::from_player(
                crate::persistence::test_fixtures::a_test_snapshot(id, 1),
            ),
            pos,
        )
        .unwrap()
    }

    fn door_on(map: &WorldMap) -> (ItemId, Option<std::num::NonZeroU16>) {
        let door = map
            .iter_items(&DOOR)
            .unwrap()
            .find(|i| i.config.name == "door")
            .unwrap();
        (door.id(), door.action_id)
    }

    #[test]
    fn closing_an_empty_doorway_swaps_the_door_and_keeps_its_action_id() {
        let (map, door) = a_doorway(&[]);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        toggle_door(&mut h.ctx(&mut map), &door, &a_door_config(1629, true)).unwrap();

        assert_eq!(
            door_on(&map),
            (ItemId(1629), std::num::NonZeroU16::new(1001))
        );
    }

    #[test]
    fn closing_on_someone_pushes_them_east() {
        let (mut map, door) = a_doorway(&[]);
        let player = a_player_at(&mut map, &DOOR, 1);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        toggle_door(&mut h.ctx(&mut map), &door, &a_door_config(1629, true)).unwrap();

        assert_eq!(map.agent_position(player), Some(&Position::new(11, 10, 7)));
        assert!(
            h.events
                .iter()
                .any(|e| matches!(e, BroadcastMessage::AgentMoved { .. }))
        );
    }

    #[test]
    fn a_blocked_east_pushes_south_instead() {
        let (mut map, door) = a_doorway(&[Position::new(11, 10, 7)]);
        let player = a_player_at(&mut map, &DOOR, 1);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        toggle_door(&mut h.ctx(&mut map), &door, &a_door_config(1629, true)).unwrap();

        assert_eq!(map.agent_position(player), Some(&Position::new(10, 11, 7)));
    }

    #[test]
    fn two_occupants_take_two_different_tiles() {
        let (mut map, door) = a_doorway(&[]);
        let first = a_player_at(&mut map, &DOOR, 1);
        let second = a_player_at(&mut map, &DOOR, 2);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        toggle_door(&mut h.ctx(&mut map), &door, &a_door_config(1629, true)).unwrap();

        assert_ne!(map.agent_position(first), map.agent_position(second));
        assert_ne!(map.agent_position(first), Some(&DOOR));
    }

    #[test]
    fn with_nowhere_to_push_the_door_stays_open_and_nobody_moves() {
        let around = [
            Position::new(11, 10, 7),
            Position::new(10, 11, 7),
            Position::new(10, 9, 7),
            Position::new(9, 10, 7),
        ];
        let (mut map, door) = a_doorway(&around);
        let player = a_player_at(&mut map, &DOOR, 1);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        let result = toggle_door(&mut h.ctx(&mut map), &door, &a_door_config(1629, true));

        assert!(matches!(result, Err(ItemActionError::NoRoom)));
        assert_eq!(map.agent_position(player), Some(&DOOR));
        assert_eq!(door_on(&map).0, ItemId(1630));
    }

    #[test]
    fn an_item_in_the_doorway_keeps_the_door_open() {
        let (mut map, door) = a_doorway(&[]);
        map.place_item(
            &DOOR,
            None,
            None,
            Item::new(
                Arc::new(ItemConfig::new(
                    ItemId(3031),
                    "gold coin".to_string(),
                    None,
                    None,
                    [ItemFlag::Take],
                    Vec::new(),
                )),
                1,
            ),
        )
        .unwrap();
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        let result = toggle_door(&mut h.ctx(&mut map), &door, &a_door_config(1629, true));

        assert!(matches!(result, Err(ItemActionError::NoRoom)));
        assert_eq!(door_on(&map).0, ItemId(1630));
    }

    #[test]
    fn opening_ignores_whoever_stands_beside_it() {
        let (mut map, door) = a_doorway(&[]);
        let neighbour = a_player_at(&mut map, &Position::new(11, 10, 7), 1);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        toggle_door(&mut h.ctx(&mut map), &door, &a_door_config(1630, false)).unwrap();

        assert_eq!(
            map.agent_position(neighbour),
            Some(&Position::new(11, 10, 7))
        );
    }

    #[test]
    fn a_decayed_item_keeps_its_action_id() {
        let config = |id| {
            Arc::new(ItemConfig::new(
                ItemId(id),
                "hole".to_string(),
                None,
                None,
                [],
                Vec::new(),
            ))
        };
        let mut hole = Item::new(config(1), 1);
        hole.action_id = std::num::NonZeroU16::new(105);

        assert_eq!(decayed_into(&hole, config(2)).action_id, hole.action_id);
    }

    #[test]
    fn a_transform_keeps_the_action_id() {
        let into = *ITEM_CONFIGS.keys().min().expect("the catalogue is empty");
        let pos = Position::new(10, 10, 7);
        let mut ground = Item::new(
            Arc::new(ItemConfig::new(
                ItemId(65000),
                "dirt".to_string(),
                None,
                None,
                [ItemFlag::Ground],
                Vec::new(),
            )),
            1,
        );
        ground.action_id = std::num::NonZeroU16::new(105);
        let guid = ground.guid;
        let mut tile = MapTile::new();
        tile.push_item(ground);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), tile);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        transform(
            &mut h.ctx(&mut map),
            &ItemRef {
                guid,
                placement: ItemPlacement::Map(pos.clone()),
            },
            into,
        )
        .expect("the transform succeeds");

        let after = map.iter_items(&pos).unwrap().next().unwrap();
        assert_eq!(after.id(), into);
        assert_eq!(after.action_id, std::num::NonZeroU16::new(105));
    }

    #[test]
    fn every_door_turns_into_a_door() {
        for config in ITEM_CONFIGS.values() {
            let Some(ItemAction::Door { new }) = config.attr_action() else {
                continue;
            };
            let into = ITEM_CONFIGS.get(&new).unwrap_or_else(|| {
                panic!(
                    "door {} turns into {new}, which is not in the catalogue",
                    config.id
                )
            });
            assert!(
                matches!(into.attr_action(), Some(ItemAction::Door { .. })),
                "door {} turns into {new}, which is not a door",
                config.id
            );
        }
    }

    #[test]
    fn every_climb_item_is_usable() {
        for config in ITEM_CONFIGS.values() {
            if matches!(config.attr_action(), Some(ItemAction::Climb(_))) {
                assert!(
                    config.has_flag(ItemFlag::Usable),
                    "{} ({}) climbs but cannot be used",
                    config.id,
                    config.name
                );
            }
        }
    }

    #[test]
    fn a_decayed_item_keeps_its_owner() {
        let config = |id| {
            Arc::new(ItemConfig::new(
                ItemId(id),
                "fire field".to_string(),
                None,
                None,
                [],
                Vec::new(),
            ))
        };
        let owner = slotmap::SlotMap::<AgentKey, ()>::with_key().insert(());
        let mut burning = Item::new(config(1), 1);
        burning.owner = Some(owner);

        let next = decayed_into(&burning, config(2));

        assert_eq!(next.id(), ItemId(2));
        assert_eq!(next.owner, Some(owner));
    }

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
        let guid = sand.guid;
        let mut tile = MapTile::new();
        tile.push_item(sand);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), tile);

        let mut h = TestHarness::new();
        let mut map = WorldMap::new(map);
        let result = transform(
            &mut h.ctx(&mut map),
            &ItemRef {
                guid,
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
