//! Pure geometry: where `DescribeMap`'s array and a `PlayerWalkAck` strip land,
//! as functions of a position, a direction and a floor. No `World`, no messages.

use rustibia_server::constants::view::{PLAYER_VIEWPORT_HEIGHT, PLAYER_VIEWPORT_WIDTH};
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
    let half_w = (PLAYER_VIEWPORT_WIDTH / 2) as i64;
    let half_h = (PLAYER_VIEWPORT_HEIGHT / 2) as i64;
    let (cx, cy) = floor_center(center, floor);
    let x_start = (cx - half_w).max(0);
    let y_start = (cy - half_h).max(0);
    let x_end = cx + half_w;
    let y_end = cy + half_h;

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
    let half_w = (PLAYER_VIEWPORT_WIDTH / 2) as i64;
    let half_h = (PLAYER_VIEWPORT_HEIGHT / 2) as i64;
    let (x, y) = floor_center(center, floor);
    let x_start = (x - half_w).max(0);
    let x_end = x + half_w;
    let y_start = (y - half_h).max(0);
    let y_end = y + half_h;

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

    #[test]
    fn describe_map_places_tiles_row_major_around_the_center() {
        let center = Position::new(200, 200, 7);
        let positions: Vec<Option<Position>> = describe_map_positions(&center, 7).collect();

        let center_index =
            (PLAYER_VIEWPORT_HEIGHT / 2) * PLAYER_VIEWPORT_WIDTH + PLAYER_VIEWPORT_WIDTH / 2;
        assert_eq!(positions[center_index], Some(center.clone()));
        assert_eq!(
            positions[center_index - PLAYER_VIEWPORT_WIDTH],
            Some(Position::new(200, 199, 7))
        );
        assert_eq!(
            positions[center_index + 1],
            Some(Position::new(201, 200, 7))
        );

        let half_w = (PLAYER_VIEWPORT_WIDTH / 2) as u16;
        let half_h = (PLAYER_VIEWPORT_HEIGHT / 2) as u16;
        assert_eq!(
            positions[0],
            Some(Position::new(200 - half_w, 200 - half_h, 7))
        );
    }

    #[test]
    fn describe_map_applies_the_floor_offset() {
        let center = Position::new(200, 200, 7);
        let positions: Vec<Option<Position>> = describe_map_positions(&center, 5).collect();

        let center_index =
            (PLAYER_VIEWPORT_HEIGHT / 2) * PLAYER_VIEWPORT_WIDTH + PLAYER_VIEWPORT_WIDTH / 2;
        // floor_offset = center.z (7) - floor (5) = 2, shifting both axes by 2.
        assert_eq!(positions[center_index], Some(Position::new(202, 202, 5)));
    }

    #[test]
    fn describe_map_skips_slots_past_the_clamped_rect_at_the_map_edge() {
        let center = Position::new(3, 3, 7);
        let positions: Vec<Option<Position>> = describe_map_positions(&center, 7).collect();

        // x_start = (3 - 9).max(0) = 0, x_end = 3 + 9 = 12: 13 of 19 columns are real.
        // y_start = (3 - 7).max(0) = 0, y_end = 3 + 7 = 10: 11 of 15 rows are real.
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

        let x_end = 100 + (PLAYER_VIEWPORT_WIDTH / 2) as u16;
        let y_start = 100 - (PLAYER_VIEWPORT_HEIGHT / 2) as u16;
        assert_eq!(positions.len(), PLAYER_VIEWPORT_HEIGHT);
        assert_eq!(positions[0], Position::new(x_end, y_start, 7));
        assert_eq!(
            positions[PLAYER_VIEWPORT_HEIGHT / 2],
            Position::new(x_end, 100, 7)
        );
        assert_eq!(
            positions[PLAYER_VIEWPORT_HEIGHT - 1],
            Position::new(x_end, y_start + PLAYER_VIEWPORT_HEIGHT as u16 - 1, 7)
        );
    }

    #[test]
    fn a_north_step_uncovers_a_west_to_east_row_at_y_start() {
        let from = Position::new(100, 100, 7);
        let positions = expansion_positions(&from, Direction::North, 7);

        let x_start = 100 - (PLAYER_VIEWPORT_WIDTH / 2) as u16;
        let y_start = 100 - (PLAYER_VIEWPORT_HEIGHT / 2) as u16;
        assert_eq!(positions.len(), PLAYER_VIEWPORT_WIDTH);
        assert_eq!(positions[0], Position::new(x_start, y_start, 7));
        assert_eq!(
            positions[PLAYER_VIEWPORT_WIDTH / 2],
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

        let x_start = 100 - (PLAYER_VIEWPORT_WIDTH / 2) as u16;
        let x_end = 100 + (PLAYER_VIEWPORT_WIDTH / 2) as u16;
        let y_start = 100 - (PLAYER_VIEWPORT_HEIGHT / 2) as u16;
        let y_end = 100 + (PLAYER_VIEWPORT_HEIGHT / 2) as u16;

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

        let x_start = 100 - (PLAYER_VIEWPORT_WIDTH / 2) as u16;
        let x_end = 100 + (PLAYER_VIEWPORT_WIDTH / 2) as u16;
        let y_start = 100 - (PLAYER_VIEWPORT_HEIGHT / 2) as u16;
        let y_end = 100 + (PLAYER_VIEWPORT_HEIGHT / 2) as u16;

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
