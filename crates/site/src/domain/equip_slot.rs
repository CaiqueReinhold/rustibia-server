use serde::Deserialize;

/// A named equipment slot in a starting kit. `index` keys `players.inventory`, and must
/// match the game server's `InventorySlot` discriminants (`crates/server/src/entities/inventory.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EquipSlot {
    Head = 1,
    Amulet = 2,
    Backpack = 3,
    Chest = 4,
    RightHand = 5,
    LeftHand = 6,
    Legs = 7,
    Feet = 8,
    Ring = 9,
    Trinket = 10,
}

impl EquipSlot {
    pub fn index(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_are_pinned_to_the_game_servers_inventory_slots() {
        let pinned = [
            (EquipSlot::Head, 1),
            (EquipSlot::Amulet, 2),
            (EquipSlot::Backpack, 3),
            (EquipSlot::Chest, 4),
            (EquipSlot::RightHand, 5),
            (EquipSlot::LeftHand, 6),
            (EquipSlot::Legs, 7),
            (EquipSlot::Feet, 8),
            (EquipSlot::Ring, 9),
            (EquipSlot::Trinket, 10),
        ];
        for (slot, index) in pinned {
            assert_eq!(slot.index(), index, "{slot:?}");
        }
    }

    #[test]
    fn slots_are_named_in_snake_case() {
        let slot: EquipSlot = serde_yaml::from_str("right_hand").unwrap();
        assert_eq!(slot, EquipSlot::RightHand);
    }
}
