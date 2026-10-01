pub mod view {
    pub const MAX_VISIBLE_ITEMS: usize = 8;
    pub const VIEW_LEFT: u16 = 8;
    pub const VIEW_RIGHT: u16 = 9;
    pub const VIEW_TOP: u16 = 6;
    pub const VIEW_BOTTOM: u16 = 7;
    pub const PLAYER_VIEWPORT_WIDTH: usize = (VIEW_LEFT + VIEW_RIGHT + 1) as usize;
    pub const PLAYER_VIEWPORT_HEIGHT: usize = (VIEW_TOP + VIEW_BOTTOM + 1) as usize;
    pub const VIEWPORT_SIZE: usize = PLAYER_VIEWPORT_HEIGHT * PLAYER_VIEWPORT_WIDTH;
    pub const SIGHT_RANGE: (u16, u16) = (8, 6);
    pub const MIN_FLOOR: u8 = 0;
    pub const MAX_FLOOR: u8 = 15;
    pub const BASE_FLOOR: u8 = 7;
    pub const UNDERGROUND_REACH: u8 = 2;
    pub const AGENT_DESPAWN_RADIUS: (u16, u16) = (38, 30);

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The pin. Its twin is `the_viewport_extents_match_the_server` in the client's
        /// `conf.rs`. A split that differs but sums to the same width still decodes, and the
        /// client draws every tile a column off.
        #[test]
        fn the_viewport_extents_match_the_client() {
            assert_eq!(
                (VIEW_LEFT, VIEW_RIGHT, VIEW_TOP, VIEW_BOTTOM),
                (8, 9, 6, 7),
                "must equal conf::map::VIEW_* in the client"
            );
        }
    }
}

pub mod movement {
    pub const SPEED_PARAM_A: f32 = 857.36;
    pub const SPEED_PARAM_B: f32 = 261.29;
    pub const SPEED_PARAM_C: f32 = -4795.009;
    pub const DIAGONAL_STEP_FACTOR: u64 = 3;
}

pub mod items {
    pub const CONTAINER_COORD_FLAG: u16 = 0xFFFF;
    pub const INVENTORY_COORD_FLAG: u16 = 0xFFFE;
    pub const CARRIED_SEARCH_FLAG: u16 = 0xFFFF;
    pub const MAX_STACK_AMOUNT: u8 = 100;

    pub const MAX_DROP_CHANCE: u32 = 100_000;

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The pin. Its twin is `carried_search_flag_matches_the_server` in the client's
        /// `conf.rs`. If these two disagree, an action-bar item use resolves to no item and is
        /// dropped in silence — nothing fails to compile and nothing errors at runtime.
        #[test]
        fn the_carried_search_flag_matches_the_client() {
            assert_eq!(
                CARRIED_SEARCH_FLAG, 0xFFFF,
                "must equal conf::map::CARRIED_SEARCH_FLAG in the client"
            );
        }
    }
}

pub mod combat {
    pub const AMMO_HIT_CEILING: u8 = 90;
    pub const THROWN_HIT_CEILING: u8 = 75;
}
