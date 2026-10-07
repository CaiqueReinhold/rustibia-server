use strum::FromRepr;

use crate::entities::items::{Item, ItemGuid};
use crate::entities::vocation::Vocation;
use crate::game::item_movement::ItemMovementError;
use std::collections::HashMap;

pub const PLAYER_DROP_SLOTS: [InventorySlot; 9] = [
    InventorySlot::Amulet,
    InventorySlot::Chest,
    InventorySlot::Feet,
    InventorySlot::Head,
    InventorySlot::LeftHand,
    InventorySlot::Legs,
    InventorySlot::RightHand,
    InventorySlot::Ring,
    InventorySlot::Trinket,
];

#[derive(Clone, Debug, PartialEq, Eq, Hash, Copy, FromRepr)]
#[repr(u8)]
pub enum InventorySlot {
    BothHands = 0,
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

impl InventorySlot {
    pub fn as_id(&self) -> u8 {
        *self as u8
    }

    pub fn from_id(id: impl TryInto<u8>) -> Option<Self> {
        Self::from_repr(id.try_into().ok()?)
    }
}

#[derive(Debug, Clone, Default)]
pub struct EquipmentStats {
    pub defense: u16,
    pub armor: u16,
    pub speed: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wearer {
    pub level: u16,
    pub vocation: Vocation,
}

#[derive(Debug, Clone)]
pub struct Inventory {
    slots: HashMap<InventorySlot, Item>,
    carried_weight: u32,
    stats: EquipmentStats,
    wearer: Wearer,
}

impl Inventory {
    pub fn from_snapshot(slots: HashMap<InventorySlot, Item>, wearer: Wearer) -> Self {
        let carried_weight = slots.values().map(|i| i.total_weight()).sum();
        let mut inventory = Inventory {
            slots,
            carried_weight,
            stats: EquipmentStats::default(),
            wearer,
        };
        inventory.update_equipment_stats();
        inventory
    }

    pub fn carried_weight(&self) -> u32 {
        self.carried_weight
    }

    pub fn set_carried_weight(&mut self, value: u32) {
        self.carried_weight = value;
    }

    pub fn stats(&self) -> &EquipmentStats {
        &self.stats
    }

    pub fn set_wearer_level(&mut self, level: u16) {
        if self.wearer.level != level {
            self.wearer.level = level;
            self.update_equipment_stats();
        }
    }

    /// Insert `item` into `slot`.
    ///
    /// - `container`: if `None`, replaces the slot item directly and returns the displaced item.
    /// - `container`: if `Some(guid)`, finds that container within the slot item and inserts there.
    ///
    /// Returns `Ok(Some(displaced))` when replacing the slot item directly,
    /// `Ok(None)` on container insert, or `Err` when the container is full/missing.
    pub fn insert(
        &mut self,
        slot: InventorySlot,
        container: Option<(&ItemGuid, usize)>,
        item: Item,
    ) -> Result<Option<Item>, ItemMovementError> {
        match container {
            None => {
                let weight_added = item.total_weight();
                let old = self.slots.insert(slot, item);
                if let Some(ref old_item) = old {
                    self.carried_weight -= old_item.total_weight();
                }
                self.carried_weight += weight_added;
                self.update_equipment_stats();
                Ok(old)
            }
            Some((target_guid, container_pos)) => {
                let slot_item = self
                    .slots
                    .get_mut(&slot)
                    .ok_or(ItemMovementError::ItemNotInPosition)?;
                let container = slot_item
                    .find_by_guid_mut(target_guid)
                    .ok_or(ItemMovementError::ItemNotInPosition)?;
                let cap = container.config.attr_capacity().unwrap();
                let content = container
                    .content
                    .as_mut()
                    .ok_or(ItemMovementError::ItemNotInPosition)?;
                if content.len() >= cap as usize {
                    return Err(ItemMovementError::ContainerIsFull);
                }
                self.carried_weight += item.total_weight();
                content.insert(container_pos, item);
                Ok(None)
            }
        }
    }

    pub fn first_available_container(&self) -> Option<&ItemGuid> {
        let backpack = self.slots.get(&InventorySlot::Backpack)?;
        find_available_container(backpack)
    }

    pub fn remove(
        &mut self,
        slot: InventorySlot,
        guid: &ItemGuid,
        amount: u8,
    ) -> Option<(Item, Option<(ItemGuid, usize)>)> {
        let slot_item = self.slots.get_mut(&slot)?;
        if slot_item.guid == *guid {
            if slot_item.amount > amount {
                let partial = slot_item.split_off(amount);
                self.carried_weight -= partial.total_weight();
                return Some((partial, None));
            } else if slot_item.amount == amount {
                let removed = self.slots.remove(&slot).unwrap();
                self.carried_weight -= removed.total_weight();
                self.update_equipment_stats();
                return Some((removed, None));
            }
            return None;
        }

        let (removed, parent) = slot_item.remove_nested(guid, amount)?;
        self.carried_weight -= removed.total_weight();
        Some((removed, Some(parent)))
    }

    /// Remove whatever item is currently in `slot`, regardless of guid.
    /// Used when evicting the current slot occupant to make room for a new item.
    pub fn take_slot(&mut self, slot: &InventorySlot) -> Option<Item> {
        let item = self.slots.remove(slot)?;
        self.carried_weight -= item.total_weight();
        self.update_equipment_stats();
        Some(item)
    }

    pub fn get(&self, slot: &InventorySlot) -> Option<&Item> {
        self.slots.get(slot)
    }

    pub fn get_mut(&mut self, slot: &InventorySlot) -> Option<&mut Item> {
        self.slots.get_mut(slot)
    }

    pub fn keys(&self) -> impl Iterator<Item = &InventorySlot> {
        self.slots.keys()
    }

    pub fn slots(&self) -> &HashMap<InventorySlot, Item> {
        &self.slots
    }

    pub fn update_equipment_stats(&mut self) {
        self.update_armor();
        self.update_defense();
        self.update_speed();
    }

    fn can_wear(&self, item: &Item) -> bool {
        item.config
            .can_be_worn_by(self.wearer.level, self.wearer.vocation)
    }

    fn worn(&self, slot: &InventorySlot) -> Option<&Item> {
        self.slots.get(slot).filter(|it| self.can_wear(it))
    }

    fn worn_items(&self) -> impl Iterator<Item = &Item> {
        self.slots.values().filter(|it| self.can_wear(it))
    }

    fn update_armor(&mut self) {
        self.stats.armor = self
            .worn_items()
            .filter_map(|it| it.config.attr_armor())
            .sum()
    }

    fn update_defense(&mut self) {
        let weapon = self.worn(&InventorySlot::LeftHand);
        let weapon_extra = weapon.and_then(|w| w.config.attr_extra_def()).unwrap_or(0);
        self.stats.defense = self
            .worn(&InventorySlot::RightHand)
            .or(weapon)
            .map(|it| (it.config.attr_defense().unwrap_or(0) as i16 + weapon_extra).max(0) as u16)
            .unwrap_or(0)
    }

    fn update_speed(&mut self) {
        self.stats.speed = self
            .worn_items()
            .filter_map(|it| it.config.attr_speed())
            .sum();
    }

    #[cfg(test)]
    pub fn total_weight(&self) -> u32 {
        self.slots.values().map(|it| it.total_weight()).sum()
    }
}

fn find_available_container(item: &Item) -> Option<&ItemGuid> {
    let available = item.available_capacity()?;
    if available > 0 {
        return Some(&item.guid);
    }
    for child in item.content.as_deref()? {
        if let Some(guid) = find_available_container(child) {
            return Some(guid);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::items::ItemId;
    use crate::entities::items::{ItemAttribute, ItemConfig, ItemFlag};
    use crate::entities::vocation::Vocation;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn anyone() -> Wearer {
        Wearer {
            level: 1,
            vocation: Vocation::Knight,
        }
    }

    fn worn_at(level: u16, slot: InventorySlot, item: Item) -> Inventory {
        Inventory::from_snapshot(
            HashMap::from([(slot, item)]),
            Wearer {
                level,
                vocation: Vocation::Knight,
            },
        )
    }

    fn gear(attributes: Vec<ItemAttribute>) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(4321),
                "gear".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Take]),
                attributes,
            )),
            1,
        )
    }

    #[test]
    fn gear_above_the_wearers_level_counts_for_nothing() {
        let helmet = gear(vec![
            ItemAttribute::Armor(8),
            ItemAttribute::RequiredLevel(20),
        ]);
        let boots = gear(vec![
            ItemAttribute::Speed(20),
            ItemAttribute::RequiredLevel(20),
        ]);

        assert_eq!(
            worn_at(19, InventorySlot::Head, helmet.clone())
                .stats()
                .armor,
            0
        );
        assert_eq!(worn_at(20, InventorySlot::Head, helmet).stats().armor, 8);
        assert_eq!(worn_at(19, InventorySlot::Feet, boots).stats().speed, 0);
    }

    #[test]
    fn gear_for_another_vocation_counts_for_nothing() {
        let robe = gear(vec![
            ItemAttribute::Armor(12),
            ItemAttribute::Vocations([Vocation::Sorcerer].into_iter().collect()),
        ]);

        assert_eq!(worn_at(100, InventorySlot::Chest, robe).stats().armor, 0);
    }

    #[test]
    fn a_shield_the_wearer_cannot_use_leaves_the_weapon_to_defend() {
        let sword = gear(vec![ItemAttribute::Defense(15), ItemAttribute::ExtraDef(2)]);
        let shield = gear(vec![
            ItemAttribute::Defense(30),
            ItemAttribute::RequiredLevel(50),
        ]);
        let inventory = Inventory::from_snapshot(
            HashMap::from([
                (InventorySlot::LeftHand, sword),
                (InventorySlot::RightHand, shield),
            ]),
            anyone(),
        );

        assert_eq!(inventory.stats().defense, 17);
    }

    #[test]
    fn reaching_the_level_turns_the_gear_on() {
        let mut inventory = worn_at(
            19,
            InventorySlot::Head,
            gear(vec![
                ItemAttribute::Armor(8),
                ItemAttribute::RequiredLevel(20),
            ]),
        );

        inventory.set_wearer_level(20);

        assert_eq!(inventory.stats().armor, 8);
    }

    /// `carried_weight` is accumulated per mutation; `total_weight` recomputes from the slots.
    /// `Player::capacity_available` reads the accumulated one, so any mutation path that lets the
    /// two disagree silently gives the player free capacity.
    fn assert_accounted(inventory: &Inventory) {
        assert_eq!(
            inventory.carried_weight,
            inventory.total_weight(),
            "carried_weight drifted from the slots it is supposed to total"
        );
    }

    fn a_thing(weight: u32, amount: u8) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1234),
                "thing".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Take, ItemFlag::Cumulative]),
                vec![ItemAttribute::Weight(weight)],
            )),
            amount,
        )
    }

    fn a_backpack() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1988),
                "backpack".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Take, ItemFlag::Container]),
                vec![ItemAttribute::Weight(18), ItemAttribute::Capacity(20)],
            )),
            1,
        )
    }

    #[test]
    fn an_empty_inventory_carries_nothing() {
        let inventory = Inventory::from_snapshot(HashMap::new(), anyone());
        assert_eq!(inventory.carried_weight, 0);
        assert_accounted(&inventory);
    }

    #[test]
    fn a_snapshot_totals_what_it_was_given() {
        let mut backpack = a_backpack();
        backpack.content = Some(Box::new(vec![a_thing(5, 3)]));
        let inventory = Inventory::from_snapshot(
            HashMap::from([
                (InventorySlot::Backpack, backpack),
                (InventorySlot::Head, a_thing(40, 1)),
            ]),
            anyone(),
        );

        assert_eq!(inventory.carried_weight, 18 + 15 + 40);
        assert_accounted(&inventory);
    }

    #[test]
    fn equipping_over_an_occupied_slot_drops_the_displaced_weight() {
        let mut inventory = Inventory::from_snapshot(
            HashMap::from([(InventorySlot::Head, a_thing(40, 1))]),
            anyone(),
        );

        let displaced = inventory
            .insert(InventorySlot::Head, None, a_thing(7, 1))
            .unwrap();

        assert!(displaced.is_some());
        assert_eq!(inventory.carried_weight, 7);
        assert_accounted(&inventory);
    }

    #[test]
    fn inserting_into_a_container_counts_through_the_nesting() {
        let backpack = a_backpack();
        let backpack_guid = backpack.guid;
        let mut inventory = Inventory::from_snapshot(
            HashMap::from([(InventorySlot::Backpack, backpack)]),
            anyone(),
        );

        inventory
            .insert(
                InventorySlot::Backpack,
                Some((&backpack_guid, 0)),
                a_thing(5, 4),
            )
            .unwrap();

        assert_eq!(inventory.carried_weight, 18 + 20);
        assert_accounted(&inventory);
    }

    #[test]
    fn a_full_container_insert_leaves_the_total_untouched() {
        let mut backpack = a_backpack();
        let backpack_guid = backpack.guid;
        backpack.content = Some(Box::new((0..20).map(|_| a_thing(1, 1)).collect()));
        let mut inventory = Inventory::from_snapshot(
            HashMap::from([(InventorySlot::Backpack, backpack)]),
            anyone(),
        );
        let before = inventory.carried_weight;

        let result = inventory.insert(
            InventorySlot::Backpack,
            Some((&backpack_guid, 0)),
            a_thing(5, 1),
        );

        assert!(matches!(result, Err(ItemMovementError::ContainerIsFull)));
        assert_eq!(inventory.carried_weight, before);
        assert_accounted(&inventory);
    }

    #[test]
    fn removing_a_whole_stack_removes_all_of_its_weight() {
        let coins = a_thing(5, 4);
        let guid = coins.guid;
        let mut inventory =
            Inventory::from_snapshot(HashMap::from([(InventorySlot::Head, coins)]), anyone());

        inventory.remove(InventorySlot::Head, &guid, 4).unwrap();

        assert_eq!(inventory.carried_weight, 0);
        assert_accounted(&inventory);
    }

    #[test]
    fn splitting_a_stack_only_removes_the_part_that_left() {
        let coins = a_thing(5, 4);
        let guid = coins.guid;
        let mut inventory =
            Inventory::from_snapshot(HashMap::from([(InventorySlot::Head, coins)]), anyone());

        let (taken, _) = inventory.remove(InventorySlot::Head, &guid, 1).unwrap();

        assert_eq!(taken.amount, 1);
        assert_eq!(inventory.carried_weight, 15);
        assert_accounted(&inventory);
    }

    #[test]
    fn removing_from_a_container_counts_through_the_nesting() {
        let mut backpack = a_backpack();
        let backpack_guid = backpack.guid;
        let coins = a_thing(5, 4);
        let coins_guid = coins.guid;
        backpack.content = Some(Box::new(vec![coins]));
        let mut inventory = Inventory::from_snapshot(
            HashMap::from([(InventorySlot::Backpack, backpack)]),
            anyone(),
        );

        inventory
            .remove(InventorySlot::Backpack, &coins_guid, 1)
            .unwrap();

        assert_eq!(inventory.carried_weight, 18 + 15);
        assert_accounted(&inventory);
        assert!(inventory.get(&InventorySlot::Backpack).unwrap().guid == backpack_guid);
    }

    #[test]
    fn taking_a_slot_drops_the_container_and_everything_in_it() {
        let mut backpack = a_backpack();
        backpack.content = Some(Box::new(vec![a_thing(5, 4)]));
        let mut inventory = Inventory::from_snapshot(
            HashMap::from([
                (InventorySlot::Backpack, backpack),
                (InventorySlot::Head, a_thing(40, 1)),
            ]),
            anyone(),
        );

        inventory.take_slot(&InventorySlot::Backpack).unwrap();

        assert_eq!(inventory.carried_weight, 40);
        assert_accounted(&inventory);
    }
}
