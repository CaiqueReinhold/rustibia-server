use crate::{
    constants::view::{
        BASE_FLOOR, MAX_FLOOR, MAX_VISIBLE_ITEMS, MIN_FLOOR, PLAYER_VIEWPORT_HEIGHT,
        PLAYER_VIEWPORT_WIDTH, UNDERGROUND_REACH, VIEWPORT_SIZE,
    },
    entities::{
        agent::AgentKey,
        inventory::InventorySlot,
        items::{ClientItemRef, ContainerId, Item, ItemGuid, ItemId},
        map::{GameMap, MapTile},
        position::{Direction, ItemPlacement, PlacementSite, Position, Rect},
    },
    local_id::LocalIdMap,
    messages::ItemStack,
};

pub fn iter_visible_floors(z: u8) -> impl Iterator<Item = u8> {
    let (min_z, max_z) = if z <= BASE_FLOOR {
        (MIN_FLOOR, BASE_FLOOR)
    } else {
        (
            z.saturating_sub(UNDERGROUND_REACH).max(BASE_FLOOR + 1),
            (z + UNDERGROUND_REACH).min(MAX_FLOOR),
        )
    };
    min_z..=max_z
}

/// The tiles `floor` contributes to a viewport centred on `viewport_center`. A
/// floor is drawn one tile up-left per floor above the centre, so the tiles it
/// covers slide down-right by the same amount. Items and agents must sweep the
/// same rectangle or they disagree by a tile per floor.
pub fn floor_viewport_rect(viewport_center: &Position, floor: u8) -> Rect {
    let half_w = (PLAYER_VIEWPORT_WIDTH / 2) as i32;
    let half_h = (PLAYER_VIEWPORT_HEIGHT / 2) as i32;
    let floor_offset = viewport_center.z as i32 - floor as i32;
    let cx = viewport_center.x as i32 + floor_offset;
    let cy = viewport_center.y as i32 + floor_offset;

    Rect::new(
        (cx - half_w).max(0) as u16,
        (cy - half_h).max(0) as u16,
        (cx + half_w).max(0) as u16,
        (cy + half_h).max(0) as u16,
    )
}

/// A tile as the wire draws it: its visible items, then `None` for the rest of the stack.
fn item_stack(tile: Option<&MapTile>) -> ItemStack {
    let mut stack: ItemStack = [None; MAX_VISIBLE_ITEMS];
    for (i, item) in tile
        .into_iter()
        .flat_map(MapTile::visible_items)
        .enumerate()
    {
        stack[i] = Some((item.id(), item.wire_subtype()));
    }
    stack
}

pub fn get_map_desc_on_viewport(
    map: &GameMap,
    viewport_center: &Position,
) -> Vec<(u8, Box<[ItemStack; VIEWPORT_SIZE]>)> {
    let mut floors = Vec::new();
    for floor in iter_visible_floors(viewport_center.z) {
        let rect = floor_viewport_rect(viewport_center, floor);
        let x_start = rect.min_x();
        let y_start = rect.min_y();

        let mut tiles = Box::new([[None; MAX_VISIBLE_ITEMS]; VIEWPORT_SIZE]);
        let mut found_any = false;
        for (pos, tile) in map.iter_tiles_in_rect(&rect, floor) {
            let col = (pos.x - x_start) as usize;
            let row = (pos.y - y_start) as usize;
            if col >= PLAYER_VIEWPORT_WIDTH || row >= PLAYER_VIEWPORT_HEIGHT {
                continue;
            }
            let stack = item_stack(tile);
            found_any |= stack[0].is_some();
            tiles[row * PLAYER_VIEWPORT_WIDTH + col] = stack;
        }
        if found_any {
            floors.push((floor, tiles));
        }
    }
    floors
}

fn expansion_rects(pos: &Position, direction: &Direction, floor: u8) -> (Rect, Option<Rect>) {
    let floor_offset = pos.z as i16 - floor as i16;
    let half_w = (PLAYER_VIEWPORT_WIDTH / 2) as i16;
    let half_h = (PLAYER_VIEWPORT_HEIGHT / 2) as i16;
    let x = pos.x as i16;
    let y = pos.y as i16;

    let x_start = (x - half_w + floor_offset).max(0) as u16;
    let x_end = (x + half_w + floor_offset) as u16;
    let y_start = (y - half_h + floor_offset).max(0) as u16;
    let y_end = (y + half_h + floor_offset) as u16;

    match direction {
        Direction::North => (Rect::new(x_start, y_start, x_end, y_start), None),
        Direction::South => (Rect::new(x_start, y_end, x_end, y_end), None),
        Direction::East => (Rect::new(x_end, y_start, x_end, y_end), None),
        Direction::West => (Rect::new(x_start, y_start, x_start, y_end), None),
        Direction::NorthEast => (
            Rect::new(x_start, y_start, x_end, y_start),
            Some(Rect::new(x_end, y_start + 1, x_end, y_end)),
        ),
        Direction::NorthWest => (
            Rect::new(x_start, y_start, x_end, y_start),
            Some(Rect::new(x_start, y_start + 1, x_start, y_end)),
        ),
        Direction::SouthEast => (
            Rect::new(x_start, y_end, x_end, y_end),
            Some(Rect::new(x_end, y_start, x_end, y_end.saturating_sub(1))),
        ),
        Direction::SouthWest => (
            Rect::new(x_start, y_end, x_end, y_end),
            Some(Rect::new(
                x_start,
                y_start,
                x_start,
                y_end.saturating_sub(1),
            )),
        ),
    }
}

pub fn get_map_expansion(
    map: &GameMap,
    viewport_center: &Position,
    direction: &Direction,
) -> Vec<(u8, Box<[ItemStack]>)> {
    let mut floors = Vec::new();
    for floor in iter_visible_floors(viewport_center.z) {
        let (rect1, rect2) = expansion_rects(viewport_center, direction, floor);
        let mut stacks = Vec::with_capacity(PLAYER_VIEWPORT_WIDTH + PLAYER_VIEWPORT_HEIGHT - 1);
        stacks.extend(
            [Some(rect1), rect2]
                .into_iter()
                .flatten()
                .flat_map(|rect| map.iter_tiles_in_rect(&rect, floor))
                .map(|(_, tile)| item_stack(tile)),
        );

        if stacks.iter().any(|stack| stack[0].is_some()) {
            floors.push((floor, stacks.into_boxed_slice()));
        }
    }
    floors
}

pub fn get_agents_in_viewport<'a>(
    map: &'a GameMap,
    position: &'a Position,
) -> impl Iterator<Item = (AgentKey, Position)> + 'a {
    iter_visible_floors(position.z).flat_map(move |floor| {
        map.iter_agents_in_rect(&floor_viewport_rect(position, floor), floor)
    })
}

pub fn get_agents_in_expansion<'a>(
    map: &'a GameMap,
    position: &'a Position,
    direction: &'a Direction,
) -> impl Iterator<Item = (AgentKey, Position)> + 'a {
    iter_visible_floors(position.z).flat_map(move |floor| {
        let (rect1, rect2) = expansion_rects(position, direction, floor);
        [Some(rect1), rect2]
            .into_iter()
            .flatten()
            .flat_map(move |rect| map.iter_agents_in_rect(&rect, floor))
    })
}

pub fn tile_stack(map: &GameMap, position: &Position) -> Box<ItemStack> {
    Box::new(item_stack(map.get_tile(position).ok()))
}

pub fn resolve_client_coord(
    position: Position,
    map: &GameMap,
    containers: &LocalIdMap<ItemGuid, ContainerId>,
    agent_key: AgentKey,
) -> Option<ItemPlacement> {
    if position.is_container_coord() {
        let guid = containers.get_global(ContainerId(position.y))?;
        let (_, within) = find_item_in_reach(map, guid, agent_key)?;
        Some(ItemPlacement::Container {
            guid: *guid,
            within: Box::new(within),
            index: position.z as usize,
        })
    } else if position.is_inventory_coord() {
        Some(ItemPlacement::Inventory(
            InventorySlot::from_id(position.y)?,
            agent_key,
        ))
    } else {
        Some(ItemPlacement::Map(position))
    }
}

pub fn retrieve_item<'a>(
    map: &'a GameMap,
    cli_item: &'a ClientItemRef,
    containers: &'a LocalIdMap<ItemGuid, ContainerId>,
    agent_key: AgentKey,
) -> Option<(&'a Item, ItemPlacement)> {
    if cli_item.position.is_carried_search_coord() {
        return find_carried_item(map, cli_item.item_id, agent_key);
    }
    let placement = resolve_client_coord(cli_item.position.clone(), map, containers, agent_key)?;
    // A map coordinate names the tile; which of its items is meant comes from the stack index.
    let item = match &placement {
        ItemPlacement::Map(pos) => map.get_item_at(pos, cli_item.stack_index as usize)?,
        other => item_at_placement(map, other)?,
    };
    (item.id() == cli_item.item_id).then_some((item, placement))
}

/// The first item of `item_id` the agent carries: its slots in slot order, each searched
/// depth-first through the containers inside it.
pub fn find_carried_item(
    map: &GameMap,
    item_id: ItemId,
    agent_key: AgentKey,
) -> Option<(&Item, ItemPlacement)> {
    let inventory = map.get_player(agent_key)?.inventory();
    let mut slots: Vec<InventorySlot> = inventory.keys().copied().collect();
    slots.sort_by_key(InventorySlot::as_id);

    slots.into_iter().find_map(|slot| {
        let item = inventory.get(&slot)?;
        let site = ItemPlacement::Inventory(slot, agent_key);
        if item.id() == item_id {
            return Some((item, site));
        }
        let (found, holder, index) = find_in_contents(item, item_id)?;
        Some((
            found,
            ItemPlacement::Container {
                guid: holder.guid,
                within: Box::new(site),
                index,
            },
        ))
    })
}

/// The first match anywhere below `container`, with the container holding it directly.
fn find_in_contents(container: &Item, item_id: ItemId) -> Option<(&Item, &Item, usize)> {
    container
        .content
        .as_ref()?
        .iter()
        .enumerate()
        .find_map(|(index, item)| {
            if item.id() == item_id {
                Some((item, container, index))
            } else {
                find_in_contents(item, item_id)
            }
        })
}

fn iter_adjacent(pos: &Position) -> impl Iterator<Item = Position> {
    let x_start = pos.x.saturating_sub(1);
    let x_end = pos.x + 1;
    let y_start = pos.y.saturating_sub(1);
    let y_end = pos.y + 1;
    let z = pos.z;

    (y_start..=y_end).flat_map(move |y| (x_start..=x_end).map(move |x| Position { x, y, z }))
}

/// The nearest site holding `guid`: the tiles around the agent first, then its own slots.
pub fn find_item_in_reach<'a>(
    map: &'a GameMap,
    guid: &ItemGuid,
    agent_key: AgentKey,
) -> Option<(&'a Item, ItemPlacement)> {
    let player_pos = map.agent_position(agent_key)?;
    let tiles = iter_adjacent(player_pos).map(ItemPlacement::Map);
    let slots = map
        .get_player(agent_key)
        .into_iter()
        .flat_map(|player| player.inventory().keys())
        .map(move |slot| ItemPlacement::Inventory(*slot, agent_key));

    tiles.chain(slots).find_map(|site| {
        let item = find_item(map, &site, guid)?;
        Some((item, site))
    })
}

pub fn find_parent_container<'a>(
    map: &'a GameMap,
    guid: &'a ItemGuid,
    agent_key: AgentKey,
) -> Option<(&'a ItemGuid, ItemPlacement)> {
    let player_pos = map.agent_position(agent_key)?;
    for pos in iter_adjacent(player_pos) {
        if let Some(parent_guid) = map.get_parent_container(&pos, guid) {
            return Some((parent_guid, ItemPlacement::Map(pos)));
        }
    }
    None
}

/// The item `guid` names, looked for at `placement`.
///
/// `Map` and `Inventory` narrow to a tile or a slot and are **searched**: a bag inside a backpack
/// is still reported by `find_item_in_reach` as sitting in that slot, so a container is located the
/// way it was found rather than by an exact path — which is why `ItemPlacement::Container` does not
/// nest. `Container` already addresses one slot, so there the guid only confirms the item has not
/// moved on.
pub fn find_item<'a>(
    map: &'a GameMap,
    placement: &ItemPlacement,
    guid: &ItemGuid,
) -> Option<&'a Item> {
    if let ItemPlacement::Container { .. } = placement {
        return item_at_placement(map, placement).filter(|it| it.guid == *guid);
    }
    match placement.site() {
        PlacementSite::Tile(pos) => map.get_item_by_id(pos, guid),
        PlacementSite::Slot(slot, agent_key) => map
            .get_player(agent_key)?
            .inventory()
            .get(&slot)?
            .find_by_guid(guid),
    }
}

/// The item a placement names outright. A `Map` placement names a tile rather than one of the
/// items on it, so it has none.
pub fn item_at_placement<'a>(map: &'a GameMap, placement: &ItemPlacement) -> Option<&'a Item> {
    match placement {
        ItemPlacement::Map(..) => None,
        ItemPlacement::Inventory(slot, agent_key) => {
            map.get_player(*agent_key)?.inventory().get(slot)
        }
        ItemPlacement::Container {
            guid,
            within,
            index,
        } => find_item(map, within, guid)?.content.as_ref()?.get(*index),
    }
}

pub enum TileEntity<'a> {
    Item(&'a Item),
    Agent(AgentKey),
}

pub fn get_top_entity<'a>(map: &'a GameMap, pos: &'a Position) -> Option<TileEntity<'a>> {
    if let Ok(last_agent) = map
        .iter_agents_at(pos)
        .map(|mut agents_iter| agents_iter.next().cloned())
        && let Some(last_agent) = last_agent
    {
        return Some(TileEntity::Agent(last_agent));
    } else if let Some(item) = map.get_top_item(pos) {
        return Some(TileEntity::Item(item));
    }

    None
}

fn walk_axis(a0: i32, b0: i32, a1: i32, b1: i32) -> impl Iterator<Item = (u16, u16)> {
    let (da, db) = (a1 - a0, b1 - b0);
    debug_assert!(
        da > 0 && db.abs() <= da,
        "caller must normalise the major axis"
    );

    let (mut q, mut r) = (0i32, 0i32);
    (a0 + 1..a1).map(move |a| {
        r += db;
        if r < 0 {
            r += da;
            q -= 1;
        } else if r >= da {
            r -= da;
            q += 1;
        }
        (a as u16, (b0 + q) as u16)
    })
}

pub fn is_sight_clear(map: &GameMap, from: &Position, to: &Position, z: u8) -> bool {
    let (dx, dy) = (to.x as i32 - from.x as i32, to.y as i32 - from.y as i32);
    if dx <= 1 && dy <= 1 {
        return true;
    }

    let steep = dy.abs() > dx.abs();
    let (a0, b0, a1, b1) = match (steep, if steep { dy > 0 } else { dx > 0 }) {
        (true, true) => (from.y as i32, from.x as i32, to.y as i32, to.x as i32),
        (true, false) => (to.y as i32, to.x as i32, from.y as i32, from.x as i32),
        (false, true) => (from.x as i32, from.y as i32, to.x as i32, to.y as i32),
        (false, false) => (to.x as i32, to.y as i32, from.x as i32, from.y as i32),
    };

    walk_axis(a0, b0, a1, b1).all(|(a, b)| {
        let (x, y) = if steep { (b, a) } else { (a, b) };
        map.has_sight(&Position::new(x, y, z))
    })
}

/// Weather a target is within bounds
pub fn can_target(from: &Position, to: &Position) -> bool {
    from.z == to.z && Rect::player_viewport(from).contains(to)
}

/// Weather a missile can travel wihout being blocked
pub fn can_throw(map: &GameMap, from: &Position, to: &Position, same_floor: bool) -> bool {
    if (from.z > 7 && to.z < 8) || (from.z < 8 && to.z > 7) {
        return false;
    }

    let floor_delta = (from.z as i8) - (to.z as i8);
    if floor_delta.abs() > 0 && same_floor {
        return false;
    }

    if floor_delta > 0 {
        // throwing at an upper level
        (from.z + 1..=to.z).all(|z| map.contains_tile(&Position::new(from.x, from.y, z)))
            && is_sight_clear(map, from, to, to.z)
    } else if floor_delta < 0 {
        // throwing at a lower level
        (from.z + 1..=to.z).all(|z| map.contains_tile(&Position::new(to.x, to.y, z)))
            && is_sight_clear(map, from, to, from.z)
    } else {
        // same floor
        is_sight_clear(map, from, to, from.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire packs three different addresses into one `Position`, distinguished only by a
    /// flag in `x`, and every item command the client sends arrives as one. Nothing else covers
    /// the decode.
    #[test]
    fn a_client_coordinate_decodes_to_the_thing_it_names() {
        use crate::constants::items::{CONTAINER_COORD_FLAG, INVENTORY_COORD_FLAG};
        use crate::entities::agent::Agent;
        use crate::entities::map::MapTile;
        use crate::persistence::test_fixtures::a_player_with_a_full_backpack;

        let at = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(at.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_player_with_a_full_backpack(1, 1)), &at)
            .unwrap();

        let backpack = map
            .get_player(key)
            .unwrap()
            .inventory()
            .get(&InventorySlot::Backpack)
            .unwrap()
            .guid;
        let mut containers: LocalIdMap<ItemGuid, ContainerId> = LocalIdMap::new();
        let open = containers.get_or_insert(backpack);

        let ground = Position::new(11, 10, 7);
        assert_eq!(
            resolve_client_coord(ground.clone(), &map, &containers, key),
            Some(ItemPlacement::Map(ground))
        );

        let slot = Position::new(
            INVENTORY_COORD_FLAG,
            InventorySlot::Backpack.as_id() as u16,
            0,
        );
        assert_eq!(
            resolve_client_coord(slot, &map, &containers, key),
            Some(ItemPlacement::Inventory(InventorySlot::Backpack, key))
        );

        assert_eq!(
            resolve_client_coord(
                Position::new(CONTAINER_COORD_FLAG, open.0, 2),
                &map,
                &containers,
                key
            ),
            Some(ItemPlacement::Container {
                guid: backpack,
                within: Box::new(ItemPlacement::Inventory(InventorySlot::Backpack, key)),
                index: 2,
            })
        );

        // A container id this session never handed out, or one whose slot it has since freed.
        assert!(
            resolve_client_coord(
                Position::new(CONTAINER_COORD_FLAG, 4242, 0),
                &map,
                &containers,
                key
            )
            .is_none()
        );
    }

    #[test]
    fn the_viewport_pairs_each_agent_with_its_own_tile() {
        use crate::entities::agent::Agent;
        use crate::entities::map::MapTile;
        use crate::persistence::test_fixtures::a_test_snapshot;

        let centre = Position::new(100, 100, 7);
        let neighbour = Position::new(103, 101, 7);
        let outside = Position::new(140, 100, 7);

        let mut map = GameMap::new();
        let seat = |map: &mut GameMap, at: &Position, id: u32| {
            map.insert_tile(at.clone(), MapTile::new());
            map.insert_agent(Agent::from_player(a_test_snapshot(id, 1)), at)
                .unwrap()
        };
        let me = seat(&mut map, &centre, 1);
        let them = seat(&mut map, &neighbour, 2);
        seat(&mut map, &outside, 3);

        let mut found: Vec<(AgentKey, Position)> = get_agents_in_viewport(&map, &centre).collect();
        found.sort_by_key(|(_, pos)| pos.x);

        assert_eq!(found, vec![(me, centre), (them, neighbour)]);
    }

    #[test]
    fn can_target_accepts_the_same_tile_and_the_viewport_edge() {
        let from = Position::new(100, 100, 7);

        assert!(can_target(&from, &Position::new(100, 100, 7)));
        assert!(can_target(&from, &Position::new(109, 107, 7)));
        assert!(can_target(&from, &Position::new(91, 93, 7)));
    }

    #[test]
    fn can_target_rejects_beyond_the_viewport() {
        let from = Position::new(100, 100, 7);

        assert!(!can_target(&from, &Position::new(110, 100, 7)));
        assert!(!can_target(&from, &Position::new(100, 108, 7)));
    }

    /// Still drawn on screen — the client viewport spans several floors — but
    /// `combat::is_in_range` can never reach it.
    #[test]
    fn can_target_rejects_another_floor() {
        let from = Position::new(100, 100, 7);

        assert!(!can_target(&from, &Position::new(101, 100, 6)));
        assert!(!can_target(&from, &Position::new(100, 100, 8)));
    }

    /// Above ground the whole surface stack is described. Nothing below
    /// `BASE_FLOOR` is included: the client hides those floors.
    #[test]
    fn the_surface_describes_every_floor_above_ground() {
        let floors: Vec<u8> = iter_visible_floors(7).collect();

        assert_eq!(floors, (MIN_FLOOR..=BASE_FLOOR).collect::<Vec<u8>>());
        assert_eq!(
            iter_visible_floors(3).collect::<Vec<u8>>(),
            (MIN_FLOOR..=BASE_FLOOR).collect::<Vec<u8>>(),
            "the window does not depend on where in the stack the player stands"
        );
    }

    /// Underground the window spans `UNDERGROUND_REACH` either side of the
    /// player, which is what the client draws.
    #[test]
    fn underground_describes_two_floors_either_side() {
        assert_eq!(
            iter_visible_floors(10).collect::<Vec<u8>>(),
            vec![8, 9, 10, 11, 12]
        );
    }

    /// Clamped at both ends, and never up into the surface stack — those floors
    /// are drawn under a different rule.
    #[test]
    fn the_underground_window_is_clamped_to_the_underground_range() {
        assert_eq!(iter_visible_floors(8).collect::<Vec<u8>>(), vec![8, 9, 10]);
        assert_eq!(
            iter_visible_floors(9).collect::<Vec<u8>>(),
            vec![8, 9, 10, 11]
        );
        assert_eq!(
            iter_visible_floors(MAX_FLOOR).collect::<Vec<u8>>(),
            vec![13, 14, 15]
        );
    }

    /// The window slides a tile per floor, in the direction that floor is drawn.
    /// `get_agents_in_viewport` and `get_map_desc_on_viewport` both go through
    /// here, so an agent standing on a described tile is always described with it.
    #[test]
    fn every_floors_window_slides_with_the_floor() {
        let center = Position::new(100, 100, 9);

        let own = floor_viewport_rect(&center, 9);
        assert_eq!((own.min_x(), own.min_y()), (91, 93));
        assert_eq!((own.max_x(), own.max_y()), (109, 107));

        // One floor up is drawn one tile up-left, so it covers the tiles one
        // down-right.
        let above = floor_viewport_rect(&center, 8);
        assert_eq!((above.min_x(), above.min_y()), (92, 94));
        assert_eq!((above.max_x(), above.max_y()), (110, 108));

        let below = floor_viewport_rect(&center, 11);
        assert_eq!((below.min_x(), below.min_y()), (89, 91));
        assert_eq!((below.max_x(), below.max_y()), (107, 105));
    }

    /// At the map's north-west corner the window clamps rather than wrapping
    /// through `u16`.
    #[test]
    fn a_window_at_the_map_corner_is_clamped() {
        let rect = floor_viewport_rect(&Position::new(2, 3, 10), 8);

        assert_eq!((rect.min_x(), rect.min_y()), (0, 0));
        assert_eq!((rect.max_x(), rect.max_y()), (13, 12));
    }

    fn carrying(snapshot: crate::persistence::player::PlayerSnapshot) -> (GameMap, AgentKey) {
        use crate::entities::agent::Agent;
        use crate::entities::map::MapTile;

        let at = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(at.clone(), MapTile::new());
        let key = map.insert_agent(Agent::from_player(snapshot), &at).unwrap();
        (map, key)
    }

    #[test]
    fn a_carried_search_finds_an_equipped_item_in_its_slot() {
        use crate::entities::items::ItemId;
        use crate::persistence::test_fixtures::a_player_with_a_full_backpack;

        let (map, key) = carrying(a_player_with_a_full_backpack(1, 1));

        let (item, placement) = find_carried_item(&map, ItemId(1988), key).unwrap();

        assert_eq!(item.id(), ItemId(1988));
        assert_eq!(
            placement,
            ItemPlacement::Inventory(InventorySlot::Backpack, key)
        );
    }

    #[test]
    fn a_carried_search_descends_through_nested_containers() {
        use crate::entities::items::ItemId;
        use crate::persistence::test_fixtures::a_player_with_a_full_backpack;

        let (map, key) = carrying(a_player_with_a_full_backpack(1, 1));
        let backpack = map
            .get_player(key)
            .unwrap()
            .inventory()
            .get(&InventorySlot::Backpack)
            .unwrap();
        let backpack_guid = backpack.guid;
        let first_pouch_guid = backpack.content.as_ref().unwrap()[0].guid;
        let in_backpack = Box::new(ItemPlacement::Inventory(InventorySlot::Backpack, key));

        let (_, third_pouch) = find_carried_item(&map, ItemId(1992), key).unwrap();
        assert_eq!(
            third_pouch,
            ItemPlacement::Container {
                guid: backpack_guid,
                within: in_backpack.clone(),
                index: 2,
            }
        );

        let (coin, coin_placement) = find_carried_item(&map, ItemId(2148), key).unwrap();
        assert_eq!(
            coin_placement,
            ItemPlacement::Container {
                guid: first_pouch_guid,
                within: in_backpack,
                index: 0,
            }
        );
        assert!(
            item_at_placement(&map, &coin_placement).is_some_and(|found| found.guid == coin.guid)
        );
    }

    #[test]
    fn a_carried_search_takes_the_lowest_slot_first() {
        use crate::entities::items::ItemId;
        use crate::persistence::test_fixtures::a_player_with_a_full_backpack;

        let mut snapshot = a_player_with_a_full_backpack(1, 1);
        let pouch = snapshot
            .inventory
            .get_mut(&InventorySlot::Backpack)
            .unwrap()
            .content
            .as_mut()
            .unwrap()
            .remove(3);
        let pouch_guid = pouch.guid;
        snapshot.inventory.insert(InventorySlot::Head, pouch);
        let (map, key) = carrying(snapshot);

        let (_, placement) = find_carried_item(&map, ItemId(2148), key).unwrap();

        assert_eq!(
            placement,
            ItemPlacement::Container {
                guid: pouch_guid,
                within: Box::new(ItemPlacement::Inventory(InventorySlot::Head, key)),
                index: 0,
            }
        );
    }

    #[test]
    fn a_carried_search_for_something_not_carried_finds_nothing() {
        use crate::entities::items::ItemId;
        use crate::persistence::test_fixtures::a_player_with_a_full_backpack;

        let (map, key) = carrying(a_player_with_a_full_backpack(1, 1));

        assert!(find_carried_item(&map, ItemId(9999), key).is_none());
    }

    #[test]
    fn retrieve_item_routes_the_search_coordinate_and_leaves_slots_alone() {
        use crate::constants::items::{CARRIED_SEARCH_FLAG, INVENTORY_COORD_FLAG};
        use crate::entities::items::ItemId;
        use crate::persistence::test_fixtures::a_player_with_a_full_backpack;

        let (map, key) = carrying(a_player_with_a_full_backpack(1, 1));
        let containers: LocalIdMap<ItemGuid, ContainerId> = LocalIdMap::new();
        let reference = |position: Position, item_id: u16| ClientItemRef {
            position,
            item_id: ItemId(item_id),
            stack_index: 0,
        };

        let search = reference(
            Position::new(INVENTORY_COORD_FLAG, CARRIED_SEARCH_FLAG, 0),
            1991,
        );
        let (item, _) = retrieve_item(&map, &search, &containers, key).unwrap();
        assert_eq!(item.id(), ItemId(1991));

        let slot = reference(
            Position::new(
                INVENTORY_COORD_FLAG,
                InventorySlot::Backpack.as_id() as u16,
                0,
            ),
            1988,
        );
        let (_, placement) = retrieve_item(&map, &slot, &containers, key).unwrap();
        assert_eq!(
            placement,
            ItemPlacement::Inventory(InventorySlot::Backpack, key)
        );
    }
}
