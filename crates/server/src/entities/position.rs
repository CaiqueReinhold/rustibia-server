use std::{
    fmt::Display,
    ops::{Add, Sub},
};

use crate::{
    constants::items::{CARRIED_SEARCH_FLAG, CONTAINER_COORD_FLAG, INVENTORY_COORD_FLAG},
    entities::{agent::AgentKey, inventory::InventorySlot, items::ItemGuid},
};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Default, serde::Deserialize)]
pub struct Position {
    pub x: u16,
    pub y: u16,
    pub z: u8,
}

impl Position {
    pub fn new(x: u16, y: u16, z: u8) -> Self {
        Self { x, y, z }
    }

    pub fn is_container_coord(&self) -> bool {
        self.x == CONTAINER_COORD_FLAG
    }

    pub fn is_inventory_coord(&self) -> bool {
        self.x == INVENTORY_COORD_FLAG
    }

    pub fn is_carried_search_coord(&self) -> bool {
        self.x == INVENTORY_COORD_FLAG && self.y == CARRIED_SEARCH_FLAG
    }

    /// Tiles between here and `other`, a diagonal counting as one step — tiles, not ticks: the
    /// unit a range is expressed in, not the cost of walking it. Ignores the floor, like
    /// `Rect::contains`.
    pub fn distance(&self, other: &Position) -> u16 {
        self.x.abs_diff(other.x).max(self.y.abs_diff(other.y))
    }

    /// Within `range` tiles of `other`, on the same floor.
    pub fn is_within(&self, other: &Position, range: u16) -> bool {
        self.z == other.z && self.distance(other) <= range
    }

    pub fn placement_is_adjacent(&self, placement: &ItemPlacement) -> bool {
        match placement.site() {
            PlacementSite::Tile(pos) => self.is_within(pos, 1),
            PlacementSite::Slot(..) => true,
        }
    }

    pub fn checked_offset(&self, dx: i32, dy: i32) -> Option<Position> {
        let x = u16::try_from(self.x as i32 + dx).ok()?;
        let y = u16::try_from(self.y as i32 + dy).ok()?;
        Some(Position::new(x, y, self.z))
    }
}

impl Add<Direction> for Position {
    type Output = Position;

    fn add(self, rhs: Direction) -> Self::Output {
        let (dx, dy) = rhs.delta();
        Self {
            x: self.x.saturating_add_signed(dx as i16),
            y: self.y.saturating_add_signed(dy as i16),
            z: self.z,
        }
    }
}

impl Sub<Direction> for Position {
    type Output = Position;

    fn sub(self, rhs: Direction) -> Self::Output {
        let (dx, dy) = rhs.delta();
        Self {
            x: self.x.saturating_sub_signed(dx as i16),
            y: self.y.saturating_sub_signed(dy as i16),
            z: self.z,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {}, {})", self.x, self.y, self.z)
    }
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum Direction {
    North,
    East,
    West,
    South,
    NorthEast,
    SouthEast,
    NorthWest,
    SouthWest,
}

impl Direction {
    pub fn is_diagonal(&self) -> bool {
        matches!(
            self,
            Direction::NorthEast
                | Direction::NorthWest
                | Direction::SouthEast
                | Direction::SouthWest
        )
    }

    pub fn delta(&self) -> (i32, i32) {
        match self {
            Direction::North => (0, -1),
            Direction::NorthEast => (1, -1),
            Direction::East => (1, 0),
            Direction::SouthEast => (1, 1),
            Direction::South => (0, 1),
            Direction::SouthWest => (-1, 1),
            Direction::West => (-1, 0),
            Direction::NorthWest => (-1, -1),
        }
    }

    pub fn from_step(dx: i32, dy: i32) -> Option<Direction> {
        ALL_DIRECTIONS
            .into_iter()
            .find(|direction| direction.delta() == (dx, dy))
    }
}

pub const ALL_DIRECTIONS: [Direction; 8] = [
    Direction::North,
    Direction::NorthEast,
    Direction::East,
    Direction::SouthEast,
    Direction::South,
    Direction::SouthWest,
    Direction::West,
    Direction::NorthWest,
];

/// Where an item sits. `Map` and `Inventory` mean *directly* on that tile or *directly* in that
/// slot; anything nested is `Container`, so a placement and a guid together name one item rather
/// than "somewhere under here".
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum ItemPlacement {
    Map(Position),
    Inventory(InventorySlot, AgentKey),

    Container {
        guid: ItemGuid,
        within: Box<ItemPlacement>,
        index: usize,
    },
}

pub enum PlacementSite<'a> {
    Tile(&'a Position),
    Slot(InventorySlot, AgentKey),
}

impl ItemPlacement {
    /// `site` as a placement of its own.
    pub fn site_placement(&self) -> ItemPlacement {
        match self.site() {
            PlacementSite::Tile(pos) => ItemPlacement::Map(pos.clone()),
            PlacementSite::Slot(slot, agent_key) => ItemPlacement::Inventory(slot, agent_key),
        }
    }

    /// The container this names, if it names one: its guid and the slot within it.
    pub fn container(&self) -> Option<(&ItemGuid, usize)> {
        match self {
            ItemPlacement::Container { guid, index, .. } => Some((guid, *index)),
            _ => None,
        }
    }

    /// The tile or slot this rests in, following the container chain to its root.
    pub fn site(&self) -> PlacementSite<'_> {
        match self {
            ItemPlacement::Map(pos) => PlacementSite::Tile(pos),
            ItemPlacement::Inventory(slot, agent_key) => PlacementSite::Slot(*slot, *agent_key),
            ItemPlacement::Container { within, .. } => within.site(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Point {
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Rect {
    min: Point,
    max: Point,
}

impl Rect {
    pub fn new(min_x: u16, min_y: u16, max_x: u16, max_y: u16) -> Self {
        Rect {
            min: Point { x: min_x, y: min_y },
            max: Point { x: max_x, y: max_y },
        }
    }

    pub fn radius(pos: &Position, radius: (u16, u16)) -> Self {
        Rect {
            min: Point {
                x: pos.x.saturating_sub(radius.0),
                y: pos.y.saturating_sub(radius.1),
            },
            max: Point {
                x: pos.x.saturating_add(radius.0),
                y: pos.y.saturating_add(radius.1),
            },
        }
    }

    /// x/y only — `Rect` has no floor, so `z` is the caller's clause.
    pub fn contains(&self, pos: &Position) -> bool {
        pos.x >= self.min.x && pos.x <= self.max.x && pos.y >= self.min.y && pos.y <= self.max.y
    }

    pub fn min_x(&self) -> u16 {
        self.min.x
    }

    pub fn min_y(&self) -> u16 {
        self.min.y
    }

    pub fn max_x(&self) -> u16 {
        self.max.x
    }

    pub fn max_y(&self) -> u16 {
        self.max.y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagonal_is_one_step_and_the_floor_is_not_counted() {
        let here = Position::new(10, 10, 7);

        assert_eq!(here.distance(&Position::new(11, 11, 7)), 1);
        assert_eq!(here.distance(&Position::new(14, 12, 7)), 4);
        assert_eq!(here.distance(&Position::new(10, 10, 0)), 0);
    }

    /// The floor check is what separates a range from a distance: `distance` ignores `z`,
    /// every predicate built on it does not.
    #[test]
    fn a_range_is_floor_scoped() {
        let here = Position::new(10, 10, 7);

        assert!(here.is_within(&Position::new(14, 10, 7), 4));
        assert!(!here.is_within(&Position::new(15, 10, 7), 4));
        assert!(!here.is_within(&Position::new(14, 10, 6), 4));
    }

    #[test]
    fn rect_contains_its_edges_but_not_beyond() {
        let rect = Rect::new(10, 20, 14, 24);

        assert!(rect.contains(&Position::new(12, 22, 7)));
        assert!(rect.contains(&Position::new(10, 20, 7)));
        assert!(rect.contains(&Position::new(14, 24, 7)));
        assert!(!rect.contains(&Position::new(9, 22, 7)));
        assert!(!rect.contains(&Position::new(15, 22, 7)));
        assert!(!rect.contains(&Position::new(12, 25, 7)));
    }

    #[test]
    fn a_radius_at_the_map_edge_clamps_instead_of_overflowing() {
        let rect = Rect::radius(&Position::new(u16::MAX, u16::MAX, 7), (9, 7));

        assert_eq!((rect.max_x(), rect.max_y()), (u16::MAX, u16::MAX));
        assert!(rect.contains(&Position::new(u16::MAX, u16::MAX, 7)));

        let origin = Rect::radius(&Position::new(0, 0, 7), (9, 7));

        assert_eq!((origin.min_x(), origin.min_y()), (0, 0));
        assert!(origin.contains(&Position::new(0, 0, 7)));
    }

    #[test]
    fn rect_contains_ignores_the_floor() {
        let rect = Rect::new(10, 20, 14, 24);

        assert!(rect.contains(&Position::new(12, 22, 0)));
        assert!(rect.contains(&Position::new(12, 22, 15)));
    }
}
