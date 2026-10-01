//! Pure geometry: where `DescribeMap`'s array and a `PlayerWalkAck` strip land,
//! as functions of a position, a direction and a floor. No `World`, no messages.

use rustibia_server::constants::view::{
    PLAYER_VIEWPORT_HEIGHT, PLAYER_VIEWPORT_WIDTH, VIEW_BOTTOM, VIEW_LEFT, VIEW_RIGHT, VIEW_TOP,
};
use rustibia_server::entities::position::{Direction, Position};

/// `center` shifted by `map_query`'s per-floor camera offset.
fn floor_center(center: &Position, floor: u8) -> (i64, i64) {
    let floor_offset = center.z as i64 - floor as i64;
    (
        center.x as i64 + floor_offset,
        center.y as i64 + floor_offset,
    )
}

fn clamp_to_u16(v: i64) -> u16 {
    v.clamp(0, u16::MAX as i64) as u16
}

/// `DescribeMap`'s `tiles` array, row-major over y then x — mirrors
/// `map_query::get_map_desc_on_viewport` (`crates/server/src/game/map_query.rs:61`).
/// `None` where the slot fell outside the rect `floor_viewport_rect` clamps to
/// near the map's edge: it was never queried and carries a placeholder, not a
/// real tile.
pub fn describe_map_positions(
    center: &Position,
    floor: u8,
) -> impl Iterator<Item = Option<Position>> {
    let (cx, cy) = floor_center(center, floor);
    let x_start = (cx - VIEW_LEFT as i64).max(0);
    let y_start = (cy - VIEW_TOP as i64).max(0);
    let x_end = cx + VIEW_RIGHT as i64;
    let y_end = cy + VIEW_BOTTOM as i64;

    (0..PLAYER_VIEWPORT_HEIGHT).flat_map(move |row| {
        (0..PLAYER_VIEWPORT_WIDTH).map(move |col| {
            let x = x_start + col as i64;
            let y = y_start + row as i64;
            (x <= x_end && y <= y_end)
                .then(|| Position::new(clamp_to_u16(x), clamp_to_u16(y), floor))
        })
    })
}

/// The strip a step in `direction` uncovers, in the order the server appends its
/// tiles — mirrors `map_query::expansion_rects` (`crates/server/src/game/map_query.rs:90`).
/// `center` is the position the step landed on, the same one `PlayerWalkAck` carries.
pub fn expansion_positions(center: &Position, direction: Direction, floor: u8) -> Vec<Position> {
    let (x, y) = floor_center(center, floor);
    let x_start = (x - VIEW_LEFT as i64).max(0);
    let x_end = x + VIEW_RIGHT as i64;
    let y_start = (y - VIEW_TOP as i64).max(0);
    let y_end = y + VIEW_BOTTOM as i64;

    let top_row = || {
        (x_start..=x_end)
            .map(move |xi| Position::new(clamp_to_u16(xi), clamp_to_u16(y_start), floor))
    };
    let bottom_row = || {
        (x_start..=x_end).map(move |xi| Position::new(clamp_to_u16(xi), clamp_to_u16(y_end), floor))
    };
    let left_col = || {
        (y_start..=y_end)
            .map(move |yi| Position::new(clamp_to_u16(x_start), clamp_to_u16(yi), floor))
    };
    let right_col = || {
        (y_start..=y_end).map(move |yi| Position::new(clamp_to_u16(x_end), clamp_to_u16(yi), floor))
    };
    let edge_len = (y_end - y_start).max(0) as usize;

    match direction {
        Direction::North => top_row().collect(),
        Direction::South => bottom_row().collect(),
        Direction::East => right_col().collect(),
        Direction::West => left_col().collect(),
        Direction::NorthEast => top_row().chain(right_col().skip(1)).collect(),
        Direction::NorthWest => top_row().chain(left_col().skip(1)).collect(),
        Direction::SouthEast => bottom_row().chain(right_col().take(edge_len)).collect(),
        Direction::SouthWest => bottom_row().chain(left_col().take(edge_len)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn center_index() -> usize {
        VIEW_TOP as usize * PLAYER_VIEWPORT_WIDTH + VIEW_LEFT as usize
    }

    /// The same table as the server's `the_window_is_eight_left_nine_right_six_up_seven_down`
    /// and the client's `map::viewport` tests. The tool may not call the server's geometry, so
    /// the numbers are the pin.
    #[test]
    fn the_described_window_matches_the_shared_table() {
        let cases = [
            (Position::new(100, 100, 7), 7, (92, 94, 109, 107)),
            (Position::new(100, 100, 7), 5, (94, 96, 111, 109)),
            (Position::new(100, 100, 9), 10, (91, 93, 108, 106)),
            (Position::new(3, 2, 7), 7, (0, 0, 12, 9)),
        ];
        for (center, floor, (min_x, min_y, max_x, max_y)) in cases {
            let described: Vec<Position> =
                describe_map_positions(&center, floor).flatten().collect();
            assert_eq!(described.first(), Some(&Position::new(min_x, min_y, floor)));
            assert_eq!(described.last(), Some(&Position::new(max_x, max_y, floor)));
            assert_eq!(
                described.len(),
                (max_x - min_x + 1) as usize * (max_y - min_y + 1) as usize
            );
        }
    }

    #[test]
    fn every_strip_lies_on_the_windows_leading_edge() {
        let center = Position::new(100, 100, 7);
        let (min_x, min_y, max_x, max_y) = (92, 94, 109, 107);
        let on_edge = |direction: &Direction, p: &Position| match direction {
            Direction::East => p.x == max_x,
            Direction::West => p.x == min_x,
            Direction::North => p.y == min_y,
            Direction::South => p.y == max_y,
            Direction::NorthEast => p.y == min_y || p.x == max_x,
            Direction::NorthWest => p.y == min_y || p.x == min_x,
            Direction::SouthEast => p.y == max_y || p.x == max_x,
            Direction::SouthWest => p.y == max_y || p.x == min_x,
        };
        let inside =
            |p: &Position| (min_x..=max_x).contains(&p.x) && (min_y..=max_y).contains(&p.y);
        for direction in [
            Direction::North,
            Direction::East,
            Direction::South,
            Direction::West,
            Direction::NorthEast,
            Direction::NorthWest,
            Direction::SouthEast,
            Direction::SouthWest,
        ] {
            let strip = expansion_positions(&center, direction, 7);
            assert!(
                strip.iter().all(|p| on_edge(&direction, p) && inside(p)),
                "{direction:?}: {strip:?}"
            );
        }
    }

    #[test]
    fn the_players_own_slot_is_left_then_top_into_the_array() {
        let center = Position::new(200, 200, 7);
        let positions: Vec<Option<Position>> = describe_map_positions(&center, 7).collect();

        assert_eq!(positions[center_index()], Some(center));
    }

    #[test]
    fn describe_map_places_tiles_row_major_around_the_center() {
        let center = Position::new(200, 200, 7);
        let positions: Vec<Option<Position>> = describe_map_positions(&center, 7).collect();

        let center_index = center_index();
        assert_eq!(positions[center_index], Some(center.clone()));
        assert_eq!(
            positions[center_index - PLAYER_VIEWPORT_WIDTH],
            Some(Position::new(200, 199, 7))
        );
        assert_eq!(
            positions[center_index + 1],
            Some(Position::new(201, 200, 7))
        );

        assert_eq!(
            positions[0],
            Some(Position::new(200 - VIEW_LEFT, 200 - VIEW_TOP, 7))
        );
    }

    #[test]
    fn describe_map_applies_the_floor_offset() {
        let center = Position::new(200, 200, 7);
        let positions: Vec<Option<Position>> = describe_map_positions(&center, 5).collect();

        let center_index = center_index();
        // floor_offset = center.z (7) - floor (5) = 2, shifting both axes by 2.
        assert_eq!(positions[center_index], Some(Position::new(202, 202, 5)));
    }

    #[test]
    fn describe_map_skips_slots_past_the_clamped_rect_at_the_map_edge() {
        let center = Position::new(3, 3, 7);
        let positions: Vec<Option<Position>> = describe_map_positions(&center, 7).collect();

        // x_start = (3 - 8).max(0) = 0, x_end = 3 + 9 = 12: 13 of 18 columns are real.
        // y_start = (3 - 6).max(0) = 0, y_end = 3 + 7 = 10: 11 of 14 rows are real.
        for row in 0..PLAYER_VIEWPORT_HEIGHT {
            for col in 0..PLAYER_VIEWPORT_WIDTH {
                let index = row * PLAYER_VIEWPORT_WIDTH + col;
                let expected =
                    (col <= 12 && row <= 10).then(|| Position::new(col as u16, row as u16, 7));
                assert_eq!(positions[index], expected, "row {row} col {col}");
            }
        }
    }

    #[test]
    fn an_east_step_uncovers_a_north_to_south_column_at_x_end() {
        let from = Position::new(100, 100, 7);
        let positions = expansion_positions(&from, Direction::East, 7);

        let x_end = 100 + VIEW_RIGHT;
        let y_start = 100 - VIEW_TOP;
        assert_eq!(positions.len(), PLAYER_VIEWPORT_HEIGHT);
        assert_eq!(positions[0], Position::new(x_end, y_start, 7));
        assert_eq!(positions[VIEW_TOP as usize], Position::new(x_end, 100, 7));
        assert_eq!(
            positions[PLAYER_VIEWPORT_HEIGHT - 1],
            Position::new(x_end, y_start + PLAYER_VIEWPORT_HEIGHT as u16 - 1, 7)
        );
    }

    #[test]
    fn a_north_step_uncovers_a_west_to_east_row_at_y_start() {
        let from = Position::new(100, 100, 7);
        let positions = expansion_positions(&from, Direction::North, 7);

        let x_start = 100 - VIEW_LEFT;
        let y_start = 100 - VIEW_TOP;
        assert_eq!(positions.len(), PLAYER_VIEWPORT_WIDTH);
        assert_eq!(positions[0], Position::new(x_start, y_start, 7));
        assert_eq!(
            positions[VIEW_LEFT as usize],
            Position::new(100, y_start, 7)
        );
        assert_eq!(
            positions[PLAYER_VIEWPORT_WIDTH - 1],
            Position::new(x_start + PLAYER_VIEWPORT_WIDTH as u16 - 1, y_start, 7)
        );
    }

    #[test]
    fn a_northeast_step_uncovers_an_l_shaped_strip() {
        let from = Position::new(100, 100, 7);
        let positions = expansion_positions(&from, Direction::NorthEast, 7);

        let x_start = 100 - VIEW_LEFT;
        let x_end = 100 + VIEW_RIGHT;
        let y_start = 100 - VIEW_TOP;
        let y_end = 100 + VIEW_BOTTOM;

        assert_eq!(
            positions.len(),
            PLAYER_VIEWPORT_WIDTH + PLAYER_VIEWPORT_HEIGHT - 1
        );
        assert_eq!(positions[0], Position::new(x_start, y_start, 7));
        assert_eq!(
            positions[PLAYER_VIEWPORT_WIDTH - 1],
            Position::new(x_end, y_start, 7)
        );
        assert_eq!(
            positions[PLAYER_VIEWPORT_WIDTH],
            Position::new(x_end, y_start + 1, 7)
        );
        assert_eq!(
            positions[PLAYER_VIEWPORT_WIDTH + PLAYER_VIEWPORT_HEIGHT - 2],
            Position::new(x_end, y_end, 7)
        );
    }

    #[test]
    fn a_southwest_step_uncovers_an_l_shaped_strip() {
        let from = Position::new(100, 100, 7);
        let positions = expansion_positions(&from, Direction::SouthWest, 7);

        let x_start = 100 - VIEW_LEFT;
        let x_end = 100 + VIEW_RIGHT;
        let y_start = 100 - VIEW_TOP;
        let y_end = 100 + VIEW_BOTTOM;

        assert_eq!(
            positions.len(),
            PLAYER_VIEWPORT_WIDTH + PLAYER_VIEWPORT_HEIGHT - 1
        );
        assert_eq!(positions[0], Position::new(x_start, y_end, 7));
        assert_eq!(
            positions[PLAYER_VIEWPORT_WIDTH - 1],
            Position::new(x_end, y_end, 7)
        );
        assert_eq!(
            positions[PLAYER_VIEWPORT_WIDTH],
            Position::new(x_start, y_start, 7)
        );
        assert_eq!(
            positions[PLAYER_VIEWPORT_WIDTH + PLAYER_VIEWPORT_HEIGHT - 2],
            Position::new(x_start, y_end - 1, 7)
        );
    }
}
