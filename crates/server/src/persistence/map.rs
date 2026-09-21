use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;
use tracing::info;

use crate::entities::items::{Item, ItemConfig, ItemFlag, ItemGuid, ItemId};
use crate::entities::map::{GameMap, MapTile};
use crate::entities::position::Position;

// ── OTBM binary stream markers ────────────────────────────────────────────────
const NODE_START: u8 = 0xFE;
const NODE_END: u8 = 0xFF;
const ESCAPE: u8 = 0xFD;

// ── Node types (classic OpenTibia / otclient OTBM format) ────────────────────
const OTBM_MAP_DATA: u8 = 0x02;
const OTBM_TILE_AREA: u8 = 0x04;
const OTBM_TILE: u8 = 0x05;
const OTBM_ITEM: u8 = 0x06;
const OTBM_HOUSETILE: u8 = 0x0E;

// ── Attribute IDs ─────────────────────────────────────────────────────────────
const ATTR_TILE_FLAGS: u8 = 0x03; // u32
const ATTR_ACTION_ID: u8 = 0x04; // u16
const ATTR_UNIQUE_ID: u8 = 0x05; // u16
const ATTR_TEXT: u8 = 0x06; // string
const ATTR_DESC: u8 = 0x07; // string (unused in most maps)
const ATTR_TELE_DEST: u8 = 0x08; // u16 x + u16 y + u8 z
const ATTR_ITEM: u8 = 0x09; // u16 item_id (inline ground item)
const ATTR_DEPOT_ID: u8 = 0x0A; // u16
const ATTR_RUNE_CHARGES: u8 = 0x0C; // u8
const ATTR_HOUSEDOORID: u8 = 0x0E; // u8
const ATTR_COUNT: u8 = 0x0F; // u8 (stackable amount)
const ATTR_DURATION: u8 = 0x10; // u32
const ATTR_DECAYING_STATE: u8 = 0x11; // u8
const ATTR_WRITTENDATE: u8 = 0x12; // u32
const ATTR_WRITTENBY: u8 = 0x13; // string
const ATTR_SLEEPERGUID: u8 = 0x14; // u32
const ATTR_SLEEPSTART: u8 = 0x15; // u32
const ATTR_CHARGES: u8 = 0x16; // u16
const ATTR_CONTAINER_ITEMS: u8 = 0x17; // u32 — item count for containers (item node context)

// ── Errors ────────────────────────────────────────────────────────────────────
#[derive(Error, Debug)]
pub enum MapRepositoryError {
    #[error("I/O error: {0}")]
    ReadError(#[from] std::io::Error),
    #[error("Unexpected end of file at offset {0:#x}")]
    UnexpectedEof(usize),
    #[error("Invalid OTBM format at offset {pos:#x} in {context}: unexpected byte {byte:#04x}")]
    InvalidFormat {
        pos: usize,
        byte: u8,
        context: &'static str,
    },
    #[error("Invalid UTF-8 string in map data")]
    InvalidString,
}

pub fn load_map(
    map_file: impl AsRef<Path>,
    items: &HashMap<ItemId, Arc<ItemConfig>>,
) -> Result<GameMap, MapRepositoryError> {
    let data = fs::read(map_file)?;
    let started = Instant::now();
    let map = parse_otbm(&data, items);
    let ended = Instant::now();
    info!(
        "Map loading took: {} secs",
        ended.duration_since(started).as_secs()
    );
    map
}

// ── Low-level parser ──────────────────────────────────────────────────────────

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(data: &'a [u8]) -> Self {
        Parser { data, pos: 0 }
    }

    fn peek_raw(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn read_raw(&mut self) -> Result<u8, MapRepositoryError> {
        self.data
            .get(self.pos)
            .copied()
            .ok_or(MapRepositoryError::UnexpectedEof(self.pos))
            .inspect(|_| self.pos += 1)
    }

    fn invalid_format(&self, byte: u8, context: &'static str) -> MapRepositoryError {
        MapRepositoryError::InvalidFormat {
            pos: self.pos - 1,
            byte,
            context,
        }
    }

    /// Read a single data byte with OTBM escape handling.
    /// Returns `None` if the next raw byte is a node boundary marker (NODE_START / NODE_END).
    fn read_data_byte(&mut self) -> Result<Option<u8>, MapRepositoryError> {
        match self.peek_raw() {
            None => Err(MapRepositoryError::UnexpectedEof(self.pos)),
            Some(NODE_START) | Some(NODE_END) => Ok(None),
            Some(ESCAPE) => {
                self.pos += 1;
                Ok(Some(self.read_raw()?))
            }
            Some(b) => {
                self.pos += 1;
                Ok(Some(b))
            }
        }
    }

    fn read_u8(&mut self) -> Result<u8, MapRepositoryError> {
        match self.read_data_byte()? {
            Some(b) => Ok(b),
            None => Err(MapRepositoryError::InvalidFormat {
                pos: self.pos,
                byte: self.data.get(self.pos).copied().unwrap_or(0),
                context: "read_u8: unexpected node boundary",
            }),
        }
    }

    fn read_u16(&mut self) -> Result<u16, MapRepositoryError> {
        let lo = self.read_u8()? as u16;
        let hi = self.read_u8()? as u16;
        Ok(lo | (hi << 8))
    }

    fn read_u32(&mut self) -> Result<u32, MapRepositoryError> {
        let b0 = self.read_u8()? as u32;
        let b1 = self.read_u8()? as u32;
        let b2 = self.read_u8()? as u32;
        let b3 = self.read_u8()? as u32;
        Ok(b0 | (b1 << 8) | (b2 << 16) | (b3 << 24))
    }

    /// Read a length-prefixed string (u16 length + bytes).
    fn read_string(&mut self) -> Result<String, MapRepositoryError> {
        let len = self.read_u16()? as usize;
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            buf.push(self.read_u8()?);
        }
        String::from_utf8(buf).map_err(|_| MapRepositoryError::InvalidString)
    }

    /// Drain all data bytes from the current node (stop at any node boundary).
    fn skip_node_data(&mut self) -> Result<(), MapRepositoryError> {
        while self.read_data_byte()?.is_some() {}
        Ok(())
    }

    /// Consume NODE_END or return an error.
    fn expect_node_end(&mut self) -> Result<(), MapRepositoryError> {
        let b = self.read_raw()?;
        if b == NODE_END {
            Ok(())
        } else {
            Err(self.invalid_format(b, "expect_node_end"))
        }
    }

    /// Skip an entire node tree (data + all children + NODE_END).
    /// Caller must have already consumed NODE_START and the node-type byte.
    fn skip_node(&mut self) -> Result<(), MapRepositoryError> {
        self.skip_node_data()?;
        while self.peek_raw() == Some(NODE_START) {
            self.pos += 1; // NODE_START
            self.read_raw()?; // node type
            self.skip_node()?;
        }
        self.expect_node_end()
    }
}

// ── High-level OTBM parsing ───────────────────────────────────────────────────

type Items = HashMap<ItemId, Arc<ItemConfig>>;

fn parse_otbm(data: &[u8], items: &Items) -> Result<GameMap, MapRepositoryError> {
    if data.len() < 5 {
        return Err(MapRepositoryError::InvalidFormat {
            pos: 0,
            byte: 0,
            context: "file too short",
        });
    }

    let mut p = Parser::new(data);

    // 4-byte file identifier (raw, not escaped — ignored)
    for _ in 0..4 {
        p.read_raw()?;
    }

    // Root node — accept any root type byte (0x00 classic otserv, 0x01 TFS/RME)
    let b = p.read_raw()?;
    if b != NODE_START {
        return Err(p.invalid_format(b, "parse_otbm: expected root NODE_START"));
    }
    let _root_type = p.read_raw()?;

    // Header: version, width, height, items major/minor
    let _version = p.read_u32()?;
    let _width = p.read_u16()?;
    let _height = p.read_u16()?;
    let _items_major = p.read_u32()?;
    let _items_minor = p.read_u32()?;

    let mut map = GameMap::new();

    // Root's children — expect OTBM_MAP_DATA
    while p.peek_raw() == Some(NODE_START) {
        p.pos += 1;
        match p.read_raw()? {
            OTBM_MAP_DATA => parse_map_data(&mut p, &mut map, items)?,
            _ => p.skip_node()?,
        }
    }

    p.expect_node_end()?;
    Ok(map)
}

fn parse_map_data(
    p: &mut Parser,
    map: &mut GameMap,
    items: &Items,
) -> Result<(), MapRepositoryError> {
    // All map data attributes are length-prefixed strings (description, spawn file, house file,
    // zones file, etc.). The exact attribute IDs vary across OTBM versions and editors, so we
    // read any attribute byte as a string rather than enumerating every possible ID.
    loop {
        match p.read_data_byte()? {
            None => break,
            Some(_) => {
                p.read_string()?;
            }
        }
    }

    // Children: tile areas (and towns / waypoints which we skip)
    while p.peek_raw() == Some(NODE_START) {
        p.pos += 1;
        match p.read_raw()? {
            OTBM_TILE_AREA => parse_tile_area(p, map, items)?,
            _ => p.skip_node()?,
        }
    }

    p.expect_node_end()
}

fn parse_tile_area(
    p: &mut Parser,
    map: &mut GameMap,
    items: &Items,
) -> Result<(), MapRepositoryError> {
    let base_x = p.read_u16()?;
    let base_y = p.read_u16()?;
    let base_z = p.read_u8()?;

    while p.peek_raw() == Some(NODE_START) {
        p.pos += 1;
        match p.read_raw()? {
            t @ (OTBM_TILE | OTBM_HOUSETILE) => {
                parse_tile(p, map, items, base_x, base_y, base_z, t)?
            }
            _ => p.skip_node()?,
        }
    }

    p.expect_node_end()
}

fn parse_tile(
    p: &mut Parser,
    map: &mut GameMap,
    items: &Items,
    base_x: u16,
    base_y: u16,
    base_z: u8,
    tile_type: u8,
) -> Result<(), MapRepositoryError> {
    let offset_x = p.read_u8()? as u16;
    let offset_y = p.read_u8()? as u16;
    let pos = Position {
        x: base_x + offset_x,
        y: base_y + offset_y,
        z: base_z,
    };

    if tile_type == OTBM_HOUSETILE {
        let _house_id = p.read_u32()?;
    }

    let mut tile = MapTile::new();

    // Tile-level attributes
    loop {
        match p.read_data_byte()? {
            None => break,
            Some(ATTR_TILE_FLAGS) => {
                p.read_u32()?;
            }
            Some(ATTR_ITEM) => {
                let item_id = ItemId(p.read_u16()?);
                tile.push_item(make_item(item_id, 1, Vec::new(), items));
            }
            Some(b) => return Err(p.invalid_format(b, "parse_tile")),
        }
    }

    // Children: items stacked on this tile
    while p.peek_raw() == Some(NODE_START) {
        p.pos += 1;
        match p.read_raw()? {
            OTBM_ITEM => tile.push_item(parse_item(p, items)?),
            _ => p.skip_node()?,
        }
    }

    map.insert_tile(pos, tile);
    p.expect_node_end()
}

fn make_item(item_id: ItemId, amount: u8, content: Vec<Item>, items: &Items) -> Item {
    let config = items
        .get(&item_id)
        .cloned()
        .expect(&format!("Map contains invalid item id: {item_id}"));
    let content = if config.has_flag(ItemFlag::Container) {
        Some(Box::new(content))
    } else {
        None
    };
    Item {
        config,
        guid: ItemGuid::new(),
        amount,
        fluid: None,
        content,
        owner: None,
    }
}

fn parse_item(p: &mut Parser, items: &Items) -> Result<Item, MapRepositoryError> {
    let item_id = ItemId(p.read_u16()?);
    let mut amount: u8 = 1;

    // Item attributes
    loop {
        match p.read_data_byte()? {
            None => break,
            Some(ATTR_COUNT) | Some(ATTR_RUNE_CHARGES) => {
                amount = p.read_u8()?;
            }
            Some(ATTR_CHARGES) => {
                // wand/weapon charges stored as u16 in TFS format
                p.read_u16()?;
            }
            Some(ATTR_ACTION_ID) | Some(ATTR_UNIQUE_ID) | Some(ATTR_DEPOT_ID) => {
                p.read_u16()?;
            }
            Some(ATTR_TEXT) | Some(ATTR_DESC) | Some(ATTR_WRITTENBY) => {
                p.read_string()?;
            }
            Some(ATTR_TELE_DEST) => {
                p.read_u16()?; // x
                p.read_u16()?; // y
                p.read_u8()?; // z
            }
            Some(ATTR_DURATION)
            | Some(ATTR_WRITTENDATE)
            | Some(ATTR_SLEEPERGUID)
            | Some(ATTR_SLEEPSTART)
            | Some(ATTR_CONTAINER_ITEMS) => {
                p.read_u32()?;
            }
            Some(ATTR_DECAYING_STATE) | Some(ATTR_HOUSEDOORID) => {
                p.read_u8()?;
            }
            Some(b) => return Err(p.invalid_format(b, "parse_item")),
        }
    }

    // Children: container contents (recursive items)
    let mut content = Vec::new();
    while p.peek_raw() == Some(NODE_START) {
        p.pos += 1;
        match p.read_raw()? {
            OTBM_ITEM => content.push(parse_item(p, items)?),
            _ => p.skip_node()?,
        }
    }

    p.expect_node_end()?;

    Ok(make_item(item_id, amount, content, items))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn an_item(id: u16) -> (ItemId, Arc<ItemConfig>) {
        (
            ItemId(id),
            Arc::new(ItemConfig::new(
                ItemId(id),
                format!("item {id}"),
                None,
                None,
                [],
                Vec::new(),
            )),
        )
    }

    /// One tile area holding a plain tile at (101, 202, 7) and a house tile at
    /// (103, 204, 7), each carrying a ground item. Every byte is chosen below
    /// 0xFD so nothing needs escaping.
    ///
    /// The node type and attribute bytes are written as literals on purpose. A
    /// fixture built from the constants it is meant to pin agrees with them
    /// whatever they say, and passes just as happily when they are wrong.
    fn a_map() -> Vec<u8> {
        let mut out: Vec<u8> = vec![0, 0, 0, 0];
        out.extend([NODE_START, 0x00]);
        out.extend(0u32.to_le_bytes());
        out.extend(100u16.to_le_bytes());
        out.extend(100u16.to_le_bytes());
        out.extend(3u32.to_le_bytes());
        out.extend(60u32.to_le_bytes());

        out.extend([NODE_START, 0x02, NODE_START, 0x04]);
        out.extend(100u16.to_le_bytes());
        out.extend(200u16.to_le_bytes());
        out.push(7);

        out.extend([NODE_START, 0x05, 1, 2, 0x09]);
        out.extend(100u16.to_le_bytes());
        out.push(NODE_END);

        out.extend([NODE_START, 0x0E, 3, 4]);
        out.extend(5u32.to_le_bytes());
        out.push(0x09);
        out.extend(101u16.to_le_bytes());
        out.push(NODE_END);

        out.extend([NODE_END, NODE_END, NODE_END]);
        out
    }

    fn ground_of(map: &GameMap, pos: Position) -> Vec<u16> {
        map.get_tile(&pos)
            .expect("tile is loaded")
            .visible_items()
            .map(|item| item.id().0)
            .collect()
    }

    /// The pin on `OTBM_HOUSETILE`. It read 0x0A for a while -- the spawn-area
    /// type -- so every house tile fell through to `skip_node` and vanished:
    /// 102k tiles in the shipped map, whole towns drawn as bare ground. Nothing
    /// errors when it happens, and `assets/map.otbm` has no house tile to catch
    /// it, which is why this builds one rather than loading a fixture file.
    #[test]
    fn a_house_tile_is_loaded_like_any_other_tile() {
        let items: Items = HashMap::from([an_item(1), an_item(100), an_item(101)]);

        let map = parse_otbm(&a_map(), &items).expect("the fixture parses");

        assert_eq!(
            ground_of(
                &map,
                Position {
                    x: 101,
                    y: 202,
                    z: 7
                }
            ),
            vec![100]
        );
        assert_eq!(
            ground_of(
                &map,
                Position {
                    x: 103,
                    y: 204,
                    z: 7
                }
            ),
            vec![101]
        );
    }
}
