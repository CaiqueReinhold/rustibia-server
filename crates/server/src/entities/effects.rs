use crate::entities::{agent::Facing, position::Position};

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, serde::Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct EffectId(pub u16);

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, serde::Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct MissileId(pub u16);

#[derive(Debug, Clone)]
pub struct Missile {
    pub missile_id: MissileId,
    pub from: Position,
    pub to: Position,
}

#[derive(Debug, Clone)]
pub struct AreaEffect {
    pub effect_id: EffectId,
    pub origin: Position,
    pub delta: Vec<(i8, i8)>,
}

impl AreaEffect {
    /// Covering only the tile it names.
    pub fn single(effect_id: EffectId, origin: Position) -> Self {
        Self {
            effect_id,
            origin,
            delta: vec![(0, 0)],
        }
    }
}

pub type AreaShapeId = String;

/// A mask and its three quarter-turns, indexed north, east, south, west.
type Rotations = [Box<[(i8, i8)]>; 4];

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct AreaShape {
    delta: Rotations,
    diagonal: Option<Rotations>,
}

impl AreaShape {
    fn rotate_cw(tiles: &[(i8, i8)]) -> Box<[(i8, i8)]> {
        tiles.iter().map(|&(dx, dy)| (-dy, dx)).collect()
    }

    fn rotations(mask: Box<[(i8, i8)]>) -> Rotations {
        let quarter = Self::rotate_cw(&mask);
        let half = Self::rotate_cw(&quarter);
        let three_quarters = Self::rotate_cw(&half);
        [mask, quarter, half, three_quarters]
    }

    pub fn new(north_facing: Box<[(i8, i8)]>) -> Self {
        AreaShape {
            delta: Self::rotations(north_facing),
            diagonal: None,
        }
    }

    /// `north_west_facing` is the mask used when a throw runs diagonally, authored for the
    /// north-west throw the way `north_facing` is authored for the northward one.
    pub fn with_diagonal(
        north_facing: Box<[(i8, i8)]>,
        north_west_facing: Box<[(i8, i8)]>,
    ) -> Self {
        AreaShape {
            delta: Self::rotations(north_facing),
            diagonal: Some(Self::rotations(north_west_facing)),
        }
    }

    pub fn get_delta(&self) -> &[(i8, i8)] {
        &self.delta[0]
    }

    pub fn get_delta_facing(&self, facing: Facing) -> &[(i8, i8)] {
        &self.delta[match facing {
            Facing::North => 0,
            Facing::East => 1,
            Facing::South => 2,
            Facing::West => 3,
        }]
    }

    pub fn get_delta_towards(&self, offset: (i32, i32), facing: Facing) -> &[(i8, i8)] {
        let (dx, dy) = offset;
        if let Some(diagonal) = &self.diagonal
            && dx != 0
            && dy != 0
        {
            return &diagonal[match (dx > 0, dy > 0) {
                (false, false) => 0,
                (true, false) => 1,
                (true, true) => 2,
                (false, true) => 3,
            }];
        }
        self.get_delta_facing(match (dx, dy) {
            (0, 0) => facing,
            (dx, _) if dx < 0 => Facing::West,
            (dx, _) if dx > 0 => Facing::East,
            (_, dy) if dy < 0 => Facing::North,
            _ => Facing::South,
        })
    }
}
