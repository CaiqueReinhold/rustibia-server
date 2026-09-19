use thiserror::Error;
use tracing::error;

use crate::{
    entities::{
        agent::AgentKey,
        combat::WeaponType,
        inventory::InventorySlot,
        items::{Item, ItemFlag, ItemRef},
        map::MapError,
        position::{ItemPlacement, PlacementSite, Rect},
    },
    game::map_query::{can_throw, find_item},
};

use super::TickCtx;
use super::events::BroadcastMessage;

#[derive(Error, Debug)]
pub enum ItemMovementError {
    #[error("Tile position does not exist")]
    TileDoesNotExist,
    #[error("Item can't be found")]
    ItemNotInPosition,
    #[error("Container is full")]
    ContainerIsFull,
    #[error("Not enough capacity")]
    NotEnoughCap,
    #[error("Player has despawned")]
    PlayerDespawned,
    #[error("Can't equip item")]
    CannotEquip,
}

fn displace_inventory_items(
    ctx: &mut TickCtx,
    agent: AgentKey,
    slot: InventorySlot,
    source_item: &Item,
    source_placement: &ItemPlacement,
    source_slot: Option<InventorySlot>,
) -> Result<(), ItemMovementError> {
    let current_item = ctx
        .map
        .player_mut(agent)
        .and_then(|mut player| player.inventory_mut().take_slot(&slot));

    // Displace any item currently in the slot back to the source
    // (inventory-to-inventory swaps are rejected upstream, so source is always Map)
    if let Some(current_item) = current_item
        && insert_item_at(ctx, current_item.clone(), source_placement, None).is_err()
    {
        let fallback = ctx.map.agent_position(agent).cloned();
        if let Some(fallback) = fallback
            && let Err(e) = insert_item_at(
                ctx,
                current_item.clone(),
                &ItemPlacement::Map(fallback),
                None,
            )
        {
            error!(
                "Failed to displace item on move. Agent: {:?}, Item: {:?}",
                agent, current_item
            );
            return Err(e);
        }
    }

    let is_bow_or_quiver = |it: &Item| {
        it.config
            .attr_weapon_type()
            .filter(|wt| matches!(wt, WeaponType::Bow | WeaponType::Crossbow))
            .is_some()
            || it.config.has_flag(ItemFlag::AmmoContainer)
    };
    let left_is_bow_or_quiver = ctx
        .map
        .get_player(agent)
        .unwrap()
        .inventory()
        .get(&InventorySlot::LeftHand)
        .map(is_bow_or_quiver)
        .unwrap_or(false);
    let source_is_bow_or_quiver = is_bow_or_quiver(source_item);
    if left_is_bow_or_quiver && source_is_bow_or_quiver {
        return Ok(());
    }

    if source_slot.unwrap() == InventorySlot::BothHands {
        let mut player = ctx.map.player_mut(agent).unwrap();
        if let Some(rh_item) = player.inventory_mut().take_slot(&InventorySlot::RightHand) {
            let rh_copy = rh_item.clone();
            if let Err(e) = stow_item(ctx, agent, rh_item) {
                let mut player = ctx.map.player_mut(agent).unwrap();
                let _ = player
                    .inventory_mut()
                    .insert(InventorySlot::RightHand, None, rh_copy);
                return Err(e);
            }
        }
    }

    let left_is_two_handed = ctx
        .map
        .get_player(agent)
        .unwrap()
        .inventory()
        .get(&InventorySlot::LeftHand)
        .map(|it| it.config.attr_inventory().unwrap() == InventorySlot::BothHands)
        .unwrap_or(false);
    if slot == InventorySlot::RightHand && left_is_two_handed {
        let mut player = ctx.map.player_mut(agent).unwrap();
        let lh_item = player
            .inventory_mut()
            .take_slot(&InventorySlot::LeftHand)
            .unwrap();
        let lh_copy = lh_item.clone();
        if let Err(e) = stow_item(ctx, agent, lh_item) {
            let mut player = ctx.map.player_mut(agent).unwrap();
            let _ = player
                .inventory_mut()
                .insert(InventorySlot::LeftHand, None, lh_copy);
            return Err(e);
        }
    }
    Ok(())
}

pub fn move_item(
    ctx: &mut TickCtx,
    agent: AgentKey,
    source: ItemRef,
    amount: u8,
    to: ItemPlacement,
) {
    if ctx.map.get_player(agent).is_none() {
        return;
    }

    let Some(player_pos) = ctx.map.agent_position(agent).cloned() else {
        return;
    };

    if let ItemPlacement::Map(pos) = &source.placement
        && !player_pos.is_within(pos, 1)
    {
        ctx.events.push(BroadcastMessage::MoveItemDenied {
            agent_key: agent,
            message: "Item is too far".to_string(),
        });
        return;
    }

    let item = find_item(ctx.map, &source.placement, &source.guid);

    // Validate source item: Unmove flag and stack amount.
    if let Some(item) = item
        && (item.config.has_flag(ItemFlag::Unmove) || item.amount < amount)
    {
        ctx.events.push(BroadcastMessage::MoveItemDenied {
            agent_key: agent,
            message: "Can't move this".to_string(),
        });
        return;
    }

    // Validate target placement.
    match (to.site(), to.container()) {
        (PlacementSite::Tile(pos), None) => {
            if !ctx.map.can_drop_item(pos)
                || !Rect::player_viewport(&player_pos).contains(pos)
                || !can_throw(ctx.map, &player_pos, pos, false)
            {
                ctx.events.push(BroadcastMessage::MoveItemDenied {
                    agent_key: agent,
                    message: "Can't drop here".to_string(),
                });
                return;
            }
        }
        (PlacementSite::Slot(target_slot, _), None) => {
            let compatible = item
                .and_then(|it| it.config.attr_inventory())
                .map(|item_slot| {
                    item_slot == target_slot
                        || (item_slot == InventorySlot::BothHands
                            && target_slot == InventorySlot::LeftHand)
                })
                .unwrap_or(false);
            if !compatible {
                ctx.events.push(BroadcastMessage::MoveItemDenied {
                    agent_key: agent,
                    message: "Can't equip this here".to_string(),
                });
                return;
            }
        }
        (_, Some((container_guid, _))) => {
            let take_ok = item
                .map(|it| it.config.has_flag(ItemFlag::Take))
                .unwrap_or(false);
            let target_is_ammo_container = find_item(ctx.map, &to.site_placement(), container_guid)
                .map(|it| it.config.has_flag(ItemFlag::AmmoContainer))
                .unwrap_or(false);
            let can_drop_to_container = item
                .filter(|_| *container_guid == source.guid)
                .filter(|_| target_is_ammo_container)
                .map(|it| it.config.attr_ammo_type().is_some())
                .unwrap_or(true);
            if !take_ok && can_drop_to_container {
                ctx.events.push(BroadcastMessage::MoveItemDenied {
                    agent_key: agent,
                    message: "Can't move this".to_string(),
                });
                return;
            }
        }
    }

    // --- Remove from source ---
    let Ok((source_item, source_index)) = remove_item_at(ctx, &source, amount) else {
        ctx.events.push(BroadcastMessage::MoveItemDenied {
            agent_key: agent,
            message: "Can't move this".to_string(),
        });
        return;
    };

    // --- Add to target ---
    let result = if let ItemPlacement::Inventory(slot, agent) = &to {
        displace_inventory_items(
            ctx,
            *agent,
            *slot,
            &source_item,
            &source.placement,
            source_item.config.attr_inventory(),
        )
    } else {
        Ok(())
    };

    let result = result.and_then(|_| insert_item_at(ctx, source_item.clone(), &to, None));

    if let Err(error) = result {
        // Restore item to its exact source position on failure
        if let Err(e) = insert_item_at(ctx, source_item.clone(), &source.placement, source_index) {
            error!(
                "Failed to revert item move. Item {:?} at {:?}. Error: {}",
                source_item, source.placement, e
            );
        }

        ctx.events.push(BroadcastMessage::MoveItemDenied {
            agent_key: agent,
            message: match error {
                ItemMovementError::ItemNotInPosition | ItemMovementError::TileDoesNotExist => {
                    "Can't move this".to_string()
                }
                e => e.to_string(),
            },
        });
    }
}

/// Into the first backpack container with room; `Err` when there is none.
pub fn stow_item(ctx: &mut TickCtx, agent: AgentKey, item: Item) -> Result<(), ItemMovementError> {
    let container = ctx
        .map
        .get_player(agent)
        .and_then(|player| player.inventory().first_available_container().cloned())
        .ok_or(ItemMovementError::CannotEquip)?;

    let mut player = ctx
        .map
        .player_mut(agent)
        .ok_or(ItemMovementError::PlayerDespawned)?;
    player
        .inventory_mut()
        .insert(InventorySlot::Backpack, Some((&container, 0)), item)?;
    Ok(())
}

/// Puts `item` where it came from: onto a like stack in that same placement when
/// one has room, otherwise as a new entry there, otherwise at the agent's feet.
pub fn return_item(
    ctx: &mut TickCtx,
    agent: AgentKey,
    placement: &ItemPlacement,
    index: Option<usize>,
    mut item: Item,
) -> Result<(), ItemMovementError> {
    if ctx.map.stack_onto(placement, &mut item) > 0 && item.amount == 0 {
        return Ok(());
    }

    let slot_taken = match placement {
        ItemPlacement::Inventory(slot, agent_key) => ctx
            .map
            .get_player(*agent_key)
            .map(|player| player.inventory().get(slot).is_some())
            .unwrap_or(true),
        _ => false,
    };

    if !slot_taken && insert_item_at(ctx, item.clone(), placement, index).is_ok() {
        return Ok(());
    }

    let pos = ctx
        .map
        .agent_position(agent)
        .cloned()
        .ok_or(ItemMovementError::PlayerDespawned)?;
    insert_item_at(ctx, item, &ItemPlacement::Map(pos), None)
}

pub fn insert_item_at(
    ctx: &mut TickCtx,
    item: Item,
    placement: &ItemPlacement,
    index: Option<usize>,
) -> Result<(), ItemMovementError> {
    let container = placement.container().map(|(g, i)| (g.clone(), i));
    match placement.site() {
        PlacementSite::Tile(pos) => {
            match ctx
                .map
                .place_item(pos, index, container.as_ref().map(|(g, i)| (g, *i)), item)
            {
                Ok(..) => {}
                Err(e) => {
                    return Err(match e {
                        MapError::ContainerIsFull => ItemMovementError::ContainerIsFull,
                        MapError::EntityNotInPosition => ItemMovementError::ItemNotInPosition,
                        MapError::TileDoesNotExist => ItemMovementError::TileDoesNotExist,
                    });
                }
            }
        }
        PlacementSite::Slot(slot, agent) => {
            let can_carry = ctx
                .map
                .get_player(agent)
                .map(|player| player.can_carry(item.total_weight()))
                .unwrap_or(false);

            if !can_carry {
                return Err(ItemMovementError::NotEnoughCap);
            }

            if let Some((c_guid, c_index)) = container.as_ref() {
                let result = ctx.map.player_mut(agent).map(|mut player| {
                    player
                        .inventory_mut()
                        .insert(slot, Some((c_guid, *c_index)), item)
                });
                let Some(result) = result else {
                    return Err(ItemMovementError::PlayerDespawned);
                };
                result?;
            } else {
                ctx.map
                    .player_mut(agent)
                    .unwrap()
                    .inventory_mut()
                    .insert(slot, None, item)?;
            }
        }
    }
    Ok(())
}

pub fn remove_item_at(
    ctx: &mut TickCtx,
    item: &ItemRef,
    amount: u8,
) -> Result<(Item, Option<usize>), ItemMovementError> {
    let removed = match item.placement.site() {
        PlacementSite::Tile(pos) => {
            let removed = ctx.map.remove_item_from_tile(pos, &item.guid, amount);
            removed.map(|(i, index, _)| (i, index))
        }
        PlacementSite::Slot(slot, agent_key) => {
            let removed = ctx
                .map
                .player_mut(agent_key)
                .and_then(|mut player| player.inventory_mut().remove(slot, &item.guid, amount));
            removed.map(|(i, _)| (i, None))
        }
    };
    removed.ok_or(ItemMovementError::ItemNotInPosition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::items::{ItemAttribute, ItemConfig};
    use crate::entities::items::{ItemGuid, ItemId};
    use crate::entities::map::GameMap;
    use crate::entities::map::MapTile;
    use crate::entities::position::Position;
    use crate::entities::world_map::WorldMap;
    use crate::game::TestHarness;
    use crate::persistence::test_fixtures::{a_player_with_a_full_backpack, a_test_snapshot};
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    fn a_movable_item(weight: u32) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1234),
                "thing".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Take]),
                [
                    ItemAttribute::Weight(weight),
                    ItemAttribute::Inventory(InventorySlot::Backpack),
                ]
                .to_vec(),
            )),
            1,
        )
    }

    /// `can_drop_item` requires a `FullBank` ground item on the tile, so every tile here
    /// gets one; a bare `MapTile::new()` rejects every drop.
    fn a_ground_tile() -> MapTile {
        let mut tile = MapTile::new();
        tile.push_item(Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1),
                "ground".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Ground, ItemFlag::FullBank]),
                Vec::new(),
            )),
            1,
        ));
        tile
    }

    /// Player at (10,10) with `item` lying on the adjacent tile (11,10), and a free
    /// tile at (12,10) to move it to.
    fn a_player_beside(item: Item) -> (GameMap, AgentKey, Position, Position, ItemGuid) {
        let (here, source, target) = (
            Position::new(10, 10, 7),
            Position::new(11, 10, 7),
            Position::new(12, 10, 7),
        );
        let guid = item.guid.clone();
        let mut map = GameMap::new();
        map.insert_tile(here.clone(), a_ground_tile());
        let mut source_tile = a_ground_tile();
        source_tile.push_item(item);
        map.insert_tile(source.clone(), source_tile);
        map.insert_tile(target.clone(), a_ground_tile());
        let agent = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();
        (map, agent, source, target, guid)
    }

    fn a_wall_tile() -> MapTile {
        let mut tile = MapTile::new();
        tile.push_item(Item::new(
            Arc::new(ItemConfig::new(
                ItemId(2),
                "wall".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Ground, ItemFlag::FullBank, ItemFlag::Unpass]),
                Vec::new(),
            )),
            1,
        ));
        tile
    }

    fn drop_onto(
        map: &mut WorldMap,
        agent: AgentKey,
        source: Position,
        guid: ItemGuid,
        to: Position,
    ) -> Vec<BroadcastMessage> {
        let mut h = TestHarness::new();
        move_item(
            &mut h.ctx(map),
            agent,
            ItemRef {
                guid,
                placement: ItemPlacement::Map(source),
            },
            1,
            ItemPlacement::Map(to),
        );
        h.events
    }

    #[test]
    fn a_drop_onto_another_floor_is_allowed_when_the_throw_is_clear() {
        let (mut map, agent, source, _, guid) = a_player_beside(a_movable_item(100));
        let above = Position::new(11, 10, 6);
        map.insert_tile(above.clone(), a_ground_tile());

        let mut map = WorldMap::new(map);
        let events = drop_onto(&mut map, agent, source, guid.clone(), above.clone());

        assert!(map.get_item_by_id(&above, &guid).is_some(), "{events:?}");
    }

    #[test]
    fn a_drop_through_a_wall_is_refused() {
        // (11,10) holds the source item, so the wall goes further along the same row and the
        // drop reaches past it.
        let (mut map, agent, source, _, guid) = a_player_beside(a_movable_item(100));
        let target = Position::new(14, 10, 7);
        map.insert_tile(Position::new(13, 10, 7), a_wall_tile());
        map.insert_tile(target.clone(), a_ground_tile());

        let mut map = WorldMap::new(map);
        let events = drop_onto(&mut map, agent, source, guid.clone(), target.clone());

        assert!(map.get_item_by_id(&target, &guid).is_none());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BroadcastMessage::MoveItemDenied { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn a_move_between_tiles_does_not_copy_the_player() {
        let mut h = TestHarness::new();
        let (map, agent, source, target, guid) = a_player_beside(a_movable_item(100));
        let before = map.clone();

        let mut map = WorldMap::new(map);
        move_item(
            &mut h.ctx(&mut map),
            agent,
            ItemRef {
                guid: guid.clone(),
                placement: ItemPlacement::Map(source),
            },
            1,
            ItemPlacement::Map(target.clone()),
        );

        assert!(map.get_item_by_id(&target, &guid).is_some());
        assert!(std::ptr::eq(
            map.get_player(agent).unwrap(),
            before.get_player(agent).unwrap()
        ));
    }

    #[test]
    fn a_move_into_the_inventory_copies_the_player_and_refreshes_capacity() {
        let mut h = TestHarness::new();
        let (map, agent, source, _, guid) = a_player_beside(a_movable_item(100));
        let before = map.clone();
        let available_before = before.get_player(agent).unwrap().capacity_available();

        let mut map = WorldMap::new(map);
        move_item(
            &mut h.ctx(&mut map),
            agent,
            ItemRef {
                guid,
                placement: ItemPlacement::Map(source),
            },
            1,
            ItemPlacement::Inventory(InventorySlot::Backpack, agent),
        );

        assert_eq!(
            map.get_player(agent).unwrap().capacity_available(),
            available_before - 100
        );
        assert!(!std::ptr::eq(
            map.get_player(agent).unwrap(),
            before.get_player(agent).unwrap()
        ));
    }

    fn an_armoured_helmet() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(4321),
                "helmet".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Take]),
                [
                    ItemAttribute::Weight(100),
                    ItemAttribute::Inventory(InventorySlot::Head),
                    ItemAttribute::Armor(8),
                ]
                .to_vec(),
            )),
            1,
        )
    }

    /// Both directions, because only one of them broke: the equip half passed while the
    /// un-equip half silently kept the armour, leaving a player protected by a helmet
    /// lying on the floor.
    #[test]
    fn equipping_and_unequipping_track_the_armour_total() {
        let mut h = TestHarness::new();
        let (map, agent, source, target, guid) = a_player_beside(an_armoured_helmet());
        assert_eq!(map.get_player(agent).unwrap().armor(), 0);

        let mut map = WorldMap::new(map);
        move_item(
            &mut h.ctx(&mut map),
            agent,
            ItemRef {
                guid: guid.clone(),
                placement: ItemPlacement::Map(source),
            },
            1,
            ItemPlacement::Inventory(InventorySlot::Head, agent),
        );

        assert_eq!(
            map.get_player(agent).unwrap().armor(),
            8,
            "equipping should count it"
        );

        move_item(
            &mut h.ctx(&mut map),
            agent,
            ItemRef {
                guid,
                placement: ItemPlacement::Inventory(InventorySlot::Head, agent),
            },
            1,
            ItemPlacement::Map(target),
        );

        assert_eq!(
            map.get_player(agent).unwrap().armor(),
            0,
            "unequipping should drop it"
        );
    }

    #[test]
    fn a_stowed_item_goes_into_the_first_free_container() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let agent = map
            .insert_agent(
                Agent::from_player(a_player_with_a_full_backpack(1, 1)),
                &pos,
            )
            .unwrap();

        let mut h = TestHarness::new();
        let item = a_flask(1);
        let guid = item.guid.clone();

        let mut map = WorldMap::new(map);
        stow_item(&mut h.ctx(&mut map), agent, item).unwrap();

        assert!(
            map.get_player(agent)
                .unwrap()
                .inventory()
                .slots()
                .values()
                .any(|slot| slot.find_by_guid(&guid).is_some()),
            "the item is not anywhere in the inventory"
        );
        assert!(map.get_top_item(&pos).is_none(), "it was dropped instead");
    }

    /// `a_test_snapshot` carries no backpack, so there is nowhere to stow anything.
    /// The refusal is the point: the caller, not this helper, decides what to do next.
    #[test]
    fn a_stowed_item_is_refused_when_there_is_nowhere_to_put_it() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let agent = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();

        let mut h = TestHarness::new();
        let item = a_flask(1);

        let mut map = WorldMap::new(map);
        assert!(matches!(
            stow_item(&mut h.ctx(&mut map), agent, item),
            Err(ItemMovementError::CannotEquip)
        ));
        assert!(map.get_top_item(&pos).is_none(), "it was dropped instead");
        assert!(h.events.is_empty());
    }

    fn a_backpack_with(capacity: u8, items: Vec<Item>) -> Item {
        let mut backpack = Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1988),
                "backpack".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Container, ItemFlag::Take]),
                [ItemAttribute::Capacity(capacity), ItemAttribute::Weight(10)].to_vec(),
            )),
            1,
        );
        backpack.content.as_mut().unwrap().extend(items);
        backpack
    }

    /// A container with a single occupied slot: `first_available_container` finds
    /// nothing, so nothing can be stowed.
    fn a_stuffed_backpack() -> Item {
        a_backpack_with(1, vec![a_movable_item(1)])
    }

    fn a_flask(amount: u8) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(4321),
                "flask".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Cumulative, ItemFlag::Take]),
                vec![ItemAttribute::Weight(160)],
            )),
            amount,
        )
    }

    fn an_item_for(slot: InventorySlot) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(2509),
                "gear".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Take]),
                vec![ItemAttribute::Weight(100), ItemAttribute::Inventory(slot)],
            )),
            1,
        )
    }

    /// The off-hand item has nowhere to go, so the equip is refused. Serving it by
    /// dropping the shield on the floor, where anyone can take it, loses the item.
    #[test]
    fn a_two_handed_equip_with_no_room_to_stow_the_off_hand_is_refused() {
        let mut h = TestHarness::new();
        let (here, source) = (Position::new(10, 10, 7), Position::new(11, 10, 7));
        let mut map = GameMap::new();
        map.insert_tile(here.clone(), a_ground_tile());
        let mut source_tile = a_ground_tile();
        let weapon = an_item_for(InventorySlot::BothHands);
        let weapon_guid = weapon.guid.clone();
        source_tile.push_item(weapon);
        map.insert_tile(source.clone(), source_tile);

        let shield = an_item_for(InventorySlot::RightHand);
        let shield_guid = shield.guid.clone();
        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.inventory = HashMap::from([
            (InventorySlot::Backpack, a_stuffed_backpack()),
            (InventorySlot::RightHand, shield),
        ]);
        let agent = map
            .insert_agent(Agent::from_player(snapshot), &here)
            .unwrap();

        let mut map = WorldMap::new(map);
        move_item(
            &mut h.ctx(&mut map),
            agent,
            ItemRef {
                guid: weapon_guid.clone(),
                placement: ItemPlacement::Map(source.clone()),
            },
            1,
            ItemPlacement::Inventory(InventorySlot::LeftHand, agent),
        );

        assert!(
            h.events
                .iter()
                .any(|b| matches!(b, BroadcastMessage::MoveItemDenied { .. })),
            "the equip was not refused: {:?}",
            h.events
        );
        let player = map.get_player(agent).unwrap();
        assert_eq!(
            player
                .inventory()
                .get(&InventorySlot::RightHand)
                .map(|it| it.guid.clone()),
            Some(shield_guid),
            "the off-hand item left the hand"
        );
        assert!(
            player.inventory().get(&InventorySlot::LeftHand).is_none(),
            "the two-hander was equipped anyway"
        );
        assert!(
            map.get_item_by_id(&source, &weapon_guid).is_some(),
            "the two-hander did not go back to the ground"
        );
        assert!(
            map.get_top_item(&here).map(|it| it.guid.clone()) != Some(weapon_guid),
            "the two-hander landed at the player's feet"
        );
    }

    /// The placement an item came from can be gone by the time it comes back -- a tile
    /// that no longer exists, say. It lands at the agent's feet rather than nowhere.
    #[test]
    fn a_returned_item_falls_to_the_agents_feet_when_its_placement_is_gone() {
        let pos = Position::new(10, 10, 7);
        let gone = Position::new(500, 500, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let agent = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();

        let mut h = TestHarness::new();
        let item = a_flask(1);
        let guid = item.guid.clone();

        let mut map = WorldMap::new(map);
        return_item(
            &mut h.ctx(&mut map),
            agent,
            &ItemPlacement::Map(gone),
            None,
            item,
        )
        .unwrap();

        assert_eq!(map.get_top_item(&pos).map(|i| i.guid.clone()), Some(guid));
    }

    /// Nothing else updates `carried_weight` for an item that never passed through
    /// `Inventory::insert`, so a merged stack would otherwise weigh nothing at all.
    #[test]
    fn a_stack_merged_into_the_inventory_still_counts_towards_the_carried_weight() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let backpack = a_backpack_with(8, vec![a_flask(1)]);
        let container_guid = backpack.guid.clone();
        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.inventory = HashMap::from([(InventorySlot::Backpack, backpack)]);
        let agent = map
            .insert_agent(Agent::from_player(snapshot), &pos)
            .unwrap();
        let before = map.get_player(agent).unwrap().inventory().carried_weight();

        let mut h = TestHarness::new();
        let mut map = WorldMap::new(map);
        return_item(
            &mut h.ctx(&mut map),
            agent,
            &ItemPlacement::Container {
                guid: container_guid,
                within: Box::new(ItemPlacement::Inventory(InventorySlot::Backpack, agent)),
                index: 0,
            },
            Some(0),
            a_flask(1),
        )
        .unwrap();

        let player = map.get_player(agent).unwrap();
        let content = player
            .inventory()
            .get(&InventorySlot::Backpack)
            .unwrap()
            .content
            .as_ref()
            .unwrap();
        assert_eq!(
            content
                .iter()
                .map(|it| (it.item_id, it.amount))
                .collect::<Vec<_>>(),
            vec![(ItemId(4321), 2)],
            "it opened a second entry instead of merging"
        );
        assert_eq!(
            player.inventory().carried_weight(),
            before + 160,
            "the merged flask weighs nothing"
        );
    }

    /// `insert_item_at` replaces whatever a bare slot holds and drops the loser, so a
    /// slot that filled up in the meantime is not a destination any more.
    #[test]
    fn a_returned_item_does_not_evict_whatever_took_its_slot() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let helmet = an_armoured_helmet();
        let helmet_guid = helmet.guid.clone();
        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.inventory = HashMap::from([(InventorySlot::Head, helmet)]);
        let agent = map
            .insert_agent(Agent::from_player(snapshot), &pos)
            .unwrap();

        let mut h = TestHarness::new();
        let item = a_flask(1);
        let guid = item.guid.clone();

        let mut map = WorldMap::new(map);
        return_item(
            &mut h.ctx(&mut map),
            agent,
            &ItemPlacement::Inventory(InventorySlot::Head, agent),
            None,
            item,
        )
        .unwrap();

        assert_eq!(
            map.get_player(agent)
                .unwrap()
                .inventory()
                .get(&InventorySlot::Head)
                .map(|it| it.guid.clone()),
            Some(helmet_guid),
            "the helmet was evicted"
        );
        assert_eq!(map.get_top_item(&pos).map(|i| i.guid.clone()), Some(guid));
    }
}
