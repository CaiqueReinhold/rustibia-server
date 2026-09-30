//! A world save's map half: the changed chunks' movable items, and putting them back at boot.

use std::collections::HashMap;
use std::sync::Arc;

use rustibia_contract::{ChunkRow, PlacedItem, StoredItemRecord, TileRow};
use tracing::warn;

use crate::entities::items::{Item, ItemConfig, ItemFlag, ItemId};
use crate::entities::map::GameMap;
use crate::game::Tick;
use crate::persistence::login::restore_item;

pub fn chunk_rows(map: &GameMap, since: Tick) -> Vec<ChunkRow> {
    map.chunks_changed_since(since)
        .map(|chunk| ChunkRow {
            cx: chunk.cx() as i32,
            cy: chunk.cy() as i32,
            z: chunk.z() as i16,
            tiles: chunk
                .tiles()
                .filter_map(|(index, tile)| {
                    let items: Vec<PlacedItem> = tile
                        .items()
                        .enumerate()
                        .filter(|(_, item)| !item.config.has_flag(ItemFlag::Unmove))
                        .filter_map(|(stack_index, item)| {
                            persistent(item).map(|item| PlacedItem {
                                stack_index: stack_index as u16,
                                item,
                            })
                        })
                        .collect();
                    (!items.is_empty()).then_some(TileRow { index, items })
                })
                .collect(),
        })
        .collect()
}

pub fn restore_chunks(
    map: &mut GameMap,
    rows: Vec<ChunkRow>,
    items: &HashMap<ItemId, Arc<ItemConfig>>,
) {
    for row in rows {
        let tiles = row
            .tiles
            .into_iter()
            .map(|tile| {
                let placed = tile
                    .items
                    .into_iter()
                    .filter_map(|placed| {
                        restore_item(items, placed.item).map(|item| (placed.stack_index, item))
                    })
                    .collect();
                (tile.index, placed)
            })
            .collect();
        if map
            .restore_chunk(row.cx as u16, row.cy as u16, row.z as u8, tiles)
            .is_err()
        {
            warn!(
                cx = row.cx,
                cy = row.cy,
                z = row.z,
                "a saved chunk is no longer on the map"
            );
        }
    }
}

fn persistent(item: &Item) -> Option<StoredItemRecord> {
    if item.config.attr_decay().is_some() {
        return None;
    }
    Some(StoredItemRecord {
        item_id: item.id().0,
        amount: item.wire_subtype(),
        content: item
            .content
            .as_ref()
            .map(|children| children.iter().filter_map(persistent).collect()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::items::ItemAttribute;
    use crate::entities::map::MapTile;
    use crate::entities::position::Position;
    use crate::entities::world_map::WorldMap;
    use crate::game::TickDelta;

    fn config(id: u16, flags: &[ItemFlag], attributes: Vec<ItemAttribute>) -> Arc<ItemConfig> {
        Arc::new(ItemConfig::new(
            ItemId(id),
            format!("item {id}"),
            None,
            None,
            flags.iter().copied(),
            attributes,
        ))
    }

    fn catalogue() -> HashMap<ItemId, Arc<ItemConfig>> {
        HashMap::from([
            (
                ItemId(100),
                config(100, &[ItemFlag::Ground, ItemFlag::Unmove], Vec::new()),
            ),
            (ItemId(200), config(200, &[ItemFlag::Take], Vec::new())),
            (
                ItemId(300),
                config(
                    300,
                    &[ItemFlag::Container],
                    vec![
                        ItemAttribute::Capacity(8),
                        ItemAttribute::Decay {
                            decay_to: ItemId(301),
                            duration: TickDelta(100),
                        },
                    ],
                ),
            ),
            (
                ItemId(400),
                config(
                    400,
                    &[ItemFlag::Container, ItemFlag::Take],
                    vec![ItemAttribute::Capacity(8)],
                ),
            ),
        ])
    }

    fn item(id: u16) -> Item {
        Item::new(Arc::clone(&catalogue()[&ItemId(id)]), 1)
    }

    fn a_map() -> (WorldMap, Position) {
        let pos = Position::new(20, 20, 7);
        let mut map = GameMap::new();
        let mut tile = MapTile::new();
        tile.push_item(item(100));
        map.insert_tile(pos.clone(), tile);
        (WorldMap::new(map), pos)
    }

    #[test]
    fn a_changed_chunk_saves_its_movable_items_that_do_not_decay_at_any_depth() {
        let (mut map, pos) = a_map();
        map.begin_tick(Tick(4));
        let mut bag = item(400);
        bag.content = Some(Box::new(vec![item(300), item(200)]));
        map.place_item(&pos, None, None, item(300)).unwrap();
        map.place_item(&pos, None, None, bag).unwrap();

        let rows = chunk_rows(&map, Tick(0));

        let stored = |id| StoredItemRecord {
            item_id: id,
            amount: 1,
            content: None,
        };
        assert_eq!(
            rows,
            vec![ChunkRow {
                cx: 1,
                cy: 1,
                z: 7,
                tiles: vec![TileRow {
                    index: 4 * 16 + 4,
                    items: vec![PlacedItem {
                        stack_index: 2,
                        item: StoredItemRecord {
                            item_id: 400,
                            amount: 1,
                            content: Some(vec![stored(200)])
                        },
                    }],
                }],
            }]
        );
    }

    #[test]
    fn a_chunk_changed_before_the_last_save_is_not_saved_again() {
        let (mut map, pos) = a_map();
        map.begin_tick(Tick(4));
        map.place_item(&pos, None, None, item(200)).unwrap();

        assert!(chunk_rows(&map, Tick(4)).is_empty());
    }

    #[test]
    fn a_chunk_emptied_of_movables_still_gets_a_row() {
        let (mut map, pos) = a_map();
        let coin = item(200);
        let guid = coin.guid;
        map.inner_mut().place_item(&pos, None, None, coin).unwrap();
        map.begin_tick(Tick(4));
        map.remove_item_from_tile(&pos, &guid, 1).unwrap();

        let rows = chunk_rows(&map, Tick(0));

        assert_eq!(rows.len(), 1);
        assert!(rows[0].tiles.is_empty());
    }

    #[test]
    fn saved_rows_restore_onto_a_fresh_map_in_the_same_order() {
        let (mut map, pos) = a_map();
        map.begin_tick(Tick(4));
        map.place_item(&pos, None, None, item(200)).unwrap();
        map.place_item(&pos, Some(1), None, item(400)).unwrap();
        let rows = chunk_rows(&map, Tick(0));
        let (fresh, _) = a_map();
        let mut fresh = fresh.snapshot();

        restore_chunks(&mut fresh, rows, &catalogue());

        let ids: Vec<u16> = fresh
            .iter_items(&pos)
            .unwrap()
            .map(|item| item.id().0)
            .collect();
        assert_eq!(ids, vec![100, 400, 200]);
    }
}
