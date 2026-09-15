use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde::Deserialize;
use thiserror::Error;

use crate::config::CONFIG;
use crate::entities::effects::{AreaShape, AreaShapeId};

pub static AREA_SHAPES: Lazy<Arc<HashMap<AreaShapeId, Arc<AreaShape>>>> = Lazy::new(|| {
    Arc::new(load_areas(&CONFIG.areas_file_path).expect("failed to load area shapes"))
});

#[derive(Error, Debug)]
pub enum AreasLoadError {
    #[error("I/O error: {0}")]
    ReadError(#[from] std::io::Error),
    #[error("YAML parse error: {0}")]
    ParseError(#[from] serde_yaml::Error),
    #[error("area `{name}` has no cells")]
    Empty { name: AreaShapeId },
    #[error("area `{name}` row {row} is {width} cells wide, but row 0 is {expected}")]
    Ragged {
        name: AreaShapeId,
        row: usize,
        width: usize,
        expected: usize,
    },
    #[error("area `{name}` has an unreadable cell {cell:?} at row {row}, column {column}")]
    UnknownCell {
        name: AreaShapeId,
        row: usize,
        column: usize,
        cell: char,
    },
    #[error("area `{name}` has no `@` marking its origin")]
    NoOrigin { name: AreaShapeId },
    #[error("area `{name}` marks a second origin at row {row}, column {column}")]
    SecondOrigin {
        name: AreaShapeId,
        row: usize,
        column: usize,
    },
    #[error("area `{name}` reaches {delta} tiles from its origin, and a wire delta is an i8")]
    OutOfReach { name: AreaShapeId, delta: isize },
}

// ── Raw YAML deserialization types ────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AreasFile {
    areas: HashMap<AreaShapeId, Vec<String>>,
}

// ── Conversion ────────────────────────────────────────────────────────────────

/// The cells that mean "not part of the shape". Both are accepted because a mask is
/// padded to a rectangle and an editor makes a space easy to lose.
const EMPTY_CELLS: [char; 2] = [' ', '.'];

/// The origin, and a tile of the shape.
const ORIGIN_INSIDE: char = '@';

/// The origin, and *not* a tile of the shape: a caster is not in their own fire wave.
const ORIGIN_OUTSIDE: char = '0';

/// A mask is read exactly as it is written: row 0 is the northmost row and column 0 the
/// westmost, so a cell *below* the `@` is a positive `dy` — the axis the map already uses.
/// A mask is therefore authored **facing north**, which is the orientation
/// `AreaShape::new` keeps at index 0: the body of a directional shape belongs above its
/// `@`, and the three remaining facings are rotations of that one.
///
/// The origin marker says whether the origin tile is itself part of the shape: `@` for one
/// that is (a burst arrow damages where it lands), `0` for one that is not (a wave leaves
/// its caster's tile alone). Both anchor the deltas; only `@` becomes a tile. This is the
/// one thing a mask says that its geometry cannot.
fn parse_shape(name: &AreaShapeId, rows: &[String]) -> Result<AreaShape, AreasLoadError> {
    let expected = rows.first().map_or(0, |row| row.chars().count());
    if expected == 0 {
        return Err(AreasLoadError::Empty { name: name.clone() });
    }

    let mut origin = None;
    let mut cells = Vec::new();

    for (row, line) in rows.iter().enumerate() {
        let width = line.chars().count();
        if width != expected {
            return Err(AreasLoadError::Ragged {
                name: name.clone(),
                row,
                width,
                expected,
            });
        }

        for (column, cell) in line.chars().enumerate() {
            match cell {
                ORIGIN_INSIDE | ORIGIN_OUTSIDE => {
                    if origin.is_some() {
                        return Err(AreasLoadError::SecondOrigin {
                            name: name.clone(),
                            row,
                            column,
                        });
                    }
                    origin = Some((row, column));
                    if cell == ORIGIN_INSIDE {
                        cells.push((row, column));
                    }
                }
                'x' | 'X' => cells.push((row, column)),
                cell if EMPTY_CELLS.contains(&cell) => {}
                cell => {
                    return Err(AreasLoadError::UnknownCell {
                        name: name.clone(),
                        row,
                        column,
                        cell,
                    });
                }
            }
        }
    }

    let (origin_row, origin_column) =
        origin.ok_or_else(|| AreasLoadError::NoOrigin { name: name.clone() })?;

    let delta = cells
        .into_iter()
        .map(|(row, column)| {
            let dx = to_delta(name, column as isize - origin_column as isize)?;
            let dy = to_delta(name, row as isize - origin_row as isize)?;
            Ok((dx, dy))
        })
        .collect::<Result<Box<[(i8, i8)]>, AreasLoadError>>()?;

    Ok(AreaShape::new(delta))
}

fn to_delta(name: &AreaShapeId, delta: isize) -> Result<i8, AreasLoadError> {
    i8::try_from(delta).map_err(|_| AreasLoadError::OutOfReach {
        name: name.clone(),
        delta,
    })
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn load_areas(
    path: impl AsRef<Path>,
) -> Result<HashMap<AreaShapeId, Arc<AreaShape>>, AreasLoadError> {
    load_areas_from_str(&fs::read_to_string(path)?)
}

fn load_areas_from_str(
    contents: &str,
) -> Result<HashMap<AreaShapeId, Arc<AreaShape>>, AreasLoadError> {
    let file: AreasFile = serde_yaml::from_str(contents)?;
    file.areas
        .iter()
        .map(|(name, rows)| Ok((name.clone(), Arc::new(parse_shape(name, rows)?))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Facing;

    fn load(contents: &str) -> HashMap<AreaShapeId, Arc<AreaShape>> {
        load_areas_from_str(contents).unwrap()
    }

    fn sorted(shape: &AreaShape, facing: Facing) -> Vec<(i8, i8)> {
        let mut delta = shape.get_delta_facing(facing).to_vec();
        delta.sort_unstable();
        delta
    }

    #[test]
    fn a_mask_reads_as_it_is_written() {
        let areas = load("areas:\n  probe:\n    - \" X \"\n    - \"X@X\"\n    - \" X \"\n");

        assert_eq!(
            areas["probe"].get_delta_facing(Facing::North),
            [(0, -1), (-1, 0), (0, 0), (1, 0), (0, 1)],
            "cells come out row-major, as (dx, dy) from the `@`"
        );
    }

    /// `@` puts the origin in the shape, `0` keeps it out, and both anchor the deltas.
    /// The distinction is the only thing a mask carries that its geometry does not, so it
    /// is worth a test on its own: read the wrong way round, every wave burns its caster
    /// and every burst arrow spares the tile it lands on.
    #[test]
    fn the_origin_marker_decides_whether_the_origin_is_a_tile() {
        let inside = load("areas:\n  point:\n    - \"X@X\"\n");
        let outside = load("areas:\n  point:\n    - \"X0X\"\n");

        assert_eq!(
            inside["point"].get_delta_facing(Facing::North),
            [(-1, 0), (0, 0), (1, 0)]
        );
        assert_eq!(
            outside["point"].get_delta_facing(Facing::North),
            [(-1, 0), (1, 0)],
            "`0` anchors the mask without joining it"
        );
    }

    /// The orientation pin. A mask is authored facing north — body above the `@` — and the
    /// other three facings are clockwise rotations of it. If this ever inverts, every
    /// directional spell fires backwards and nothing fails to compile.
    #[test]
    fn a_mask_is_authored_facing_north_and_rotates_clockwise() {
        let areas = load("areas:\n  spike:\n    - \" X \"\n    - \" X \"\n    - \" @ \"\n");
        let spike = &areas["spike"];

        assert_eq!(sorted(spike, Facing::North), [(0, -2), (0, -1), (0, 0)]);
        assert_eq!(sorted(spike, Facing::East), [(0, 0), (1, 0), (2, 0)]);
        assert_eq!(sorted(spike, Facing::South), [(0, 0), (0, 1), (0, 2)]);
        assert_eq!(sorted(spike, Facing::West), [(-2, 0), (-1, 0), (0, 0)]);
    }

    #[test]
    fn a_dot_is_empty_like_a_space() {
        let areas = load("areas:\n  dotted:\n    - \"..X..\"\n    - \"..@..\"\n");

        assert_eq!(
            areas["dotted"].get_delta_facing(Facing::North),
            [(0, -1), (0, 0)]
        );
    }

    #[test]
    fn a_ragged_mask_is_refused_rather_than_padded() {
        let error = load_areas_from_str("areas:\n  ragged:\n    - \"XXX\"\n    - \"X@\"\n")
            .expect_err("a short row changes the shape and must not pass");

        assert!(
            matches!(
                error,
                AreasLoadError::Ragged {
                    row: 1,
                    width: 2,
                    expected: 3,
                    ..
                }
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_mask_without_an_origin_is_refused() {
        let error = load_areas_from_str("areas:\n  headless:\n    - \"XXX\"\n")
            .expect_err("there is nothing to take deltas from");

        assert!(
            matches!(error, AreasLoadError::NoOrigin { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_mask_with_two_origins_is_refused() {
        let error = load_areas_from_str("areas:\n  twins:\n    - \"@@\"\n")
            .expect_err("two origins mean two shapes");

        assert!(
            matches!(
                error,
                AreasLoadError::SecondOrigin {
                    row: 0,
                    column: 1,
                    ..
                }
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_unreadable_cell_is_refused() {
        let error = load_areas_from_str("areas:\n  typo:\n    - \"o@\"\n")
            .expect_err("an unknown cell is a mask meaning something the loader cannot read");

        assert!(
            matches!(error, AreasLoadError::UnknownCell { cell: 'o', .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_shipped_catalogue_loads() {
        load_areas(&CONFIG.areas_file_path).unwrap();
    }

    /// A symmetric shape must come out of the rotation unchanged. This is what catches a
    /// rotation that mirrors instead of turning: a directional mask would still look
    /// plausible, a circle would not.
    #[test]
    fn a_symmetric_shape_is_rotation_invariant() {
        let areas = load(
            "areas:\n  ring:\n    - \" XXX \"\n    - \"XXXXX\"\n    - \"XX@XX\"\n    - \"XXXXX\"\n    - \" XXX \"\n",
        );
        let ring = &areas["ring"];
        let north = sorted(ring, Facing::North);

        for facing in [Facing::East, Facing::South, Facing::West] {
            assert_eq!(sorted(ring, facing), north, "facing {facing:?}");
        }
    }
}
