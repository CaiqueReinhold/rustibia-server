use std::collections::HashMap;
use std::sync::Arc;

use crate::entities::{
    combat::{AmmoType, CombatElement, WeaponType},
    effects::{AreaShape, EffectId},
    inventory::{Inventory, InventorySlot, Wearer},
    items::{Item, ItemFlag},
    position::Position,
    skills::{SkillType, SkillValue},
    vocation::Vocation,
};

/// A character's database identity.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[repr(transparent)]
pub struct PlayerId(pub u32);

impl std::fmt::Display for PlayerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug)]
pub struct Player {
    id: PlayerId,
    name: String,
    account_id: i32,
    admin: bool,
    last_logout_position: Position,
    vocation: Vocation,
    capacity: u32,
    inventory: Arc<Inventory>,
    skills: HashMap<SkillType, SkillValue>,
}

impl Player {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PlayerId,
        name: String,
        account_id: i32,
        admin: bool,
        last_logout_position: Position,
        vocation: Vocation,
        capacity: u32,
        inventory: HashMap<InventorySlot, Item>,
        skills: HashMap<SkillType, SkillValue>,
    ) -> Self {
        let level = skills
            .get(&SkillType::Level)
            .map_or(SkillType::Level.default_value(), |skill| skill.value);
        Self {
            id,
            name,
            account_id,
            admin,
            last_logout_position,
            vocation,
            capacity,
            inventory: Arc::new(Inventory::from_snapshot(
                inventory,
                Wearer { level, vocation },
            )),
            skills,
        }
    }

    pub fn id(&self) -> PlayerId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn account_id(&self) -> i32 {
        self.account_id
    }

    pub fn admin(&self) -> bool {
        self.admin
    }

    pub fn last_logout_position(&self) -> &Position {
        &self.last_logout_position
    }

    pub fn vocation(&self) -> Vocation {
        self.vocation
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    pub fn set_capacity(&mut self, capacity: u32) {
        self.capacity = capacity;
    }

    pub fn armor(&self) -> u16 {
        self.inventory.stats().armor
    }

    pub fn defense(&self) -> u16 {
        self.inventory.stats().defense
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn inventory_mut(&mut self) -> &mut Inventory {
        Arc::make_mut(&mut self.inventory)
    }

    pub fn skills(&self) -> &HashMap<SkillType, SkillValue> {
        &self.skills
    }

    pub fn skills_mut(&mut self) -> &mut HashMap<SkillType, SkillValue> {
        &mut self.skills
    }

    pub fn capacity_available(&self) -> u32 {
        self.capacity
            .saturating_sub(self.inventory.carried_weight())
    }

    pub fn can_carry(&self, additional_weight: u32) -> bool {
        self.inventory.carried_weight() + additional_weight <= self.capacity
    }

    pub fn has_shield(&self) -> bool {
        self.inventory
            .get(&InventorySlot::RightHand)
            .filter(|it| self.can_use(it))
            .is_some_and(|it| it.config.attr_defense().is_some())
    }

    pub fn weapon(&self) -> Option<&Item> {
        self.inventory
            .get(&InventorySlot::LeftHand)
            .filter(|it| self.can_use(it))
    }

    pub fn weapon_element(&self) -> CombatElement {
        self.weapon_ammo()
            .and_then(|it| it.config.attr_weapon_element())
            .or_else(|| self.weapon().and_then(|it| it.config.attr_weapon_element()))
            .unwrap_or(CombatElement::Physical)
    }

    pub fn weapon_attack(&self) -> u16 {
        self.weapon_ammo()
            .and_then(|it| it.config.attr_weapon_attack())
            .or_else(|| self.weapon().and_then(|it| it.config.attr_weapon_attack()))
            .unwrap_or(if matches!(self.weapon_type(), WeaponType::None) {
                5
            } else {
                0
            })
    }

    pub fn weapon_type(&self) -> WeaponType {
        self.weapon()
            .and_then(|it| it.config.attr_weapon_type())
            .unwrap_or(WeaponType::None)
    }

    pub fn weapon_ammo(&self) -> Option<&Item> {
        let weapon_type = self.weapon_type();
        if matches!(weapon_type, WeaponType::Crossbow | WeaponType::Bow) {
            self.inventory
                .get(&InventorySlot::RightHand)
                .filter(|it| it.config.has_flag(ItemFlag::AmmoContainer) && self.can_use(it))
                .and_then(|quiv| {
                    quiv.content.as_ref().and_then(|content| {
                        content.iter().find(|it| {
                            self.can_use(it)
                                && it.config.attr_ammo_type().is_some_and(|at| {
                                    matches!(
                                        (at, weapon_type),
                                        (AmmoType::Arrow, WeaponType::Bow)
                                            | (AmmoType::Bolt, WeaponType::Crossbow)
                                    )
                                })
                        })
                    })
                })
        } else {
            None
        }
    }

    pub fn weapon_range(&self) -> u8 {
        self.weapon()
            .and_then(|it| it.config.attr_weapon_range())
            .unwrap_or(1)
    }

    pub fn weapon_mana_cost(&self) -> u32 {
        self.weapon()
            .and_then(|it| it.config.attr_mana_cost())
            .unwrap_or(0)
    }

    pub fn weapon_area(&self) -> Option<(&AreaShape, EffectId)> {
        let wp = self.weapon().and_then(|it| it.config.attr_weapon_area());
        self.weapon_ammo()
            .map_or(wp, |it| it.config.attr_weapon_area())
    }

    pub fn skill(&self, skill: SkillType) -> u16 {
        let default = skill.default_value();
        self.skills.get(&skill).map_or(default, |st| st.value)
    }

    pub fn level(&self) -> u16 {
        self.skill(SkillType::Level)
    }

    pub fn can_use(&self, item: &Item) -> bool {
        item.config.can_be_worn_by(self.level(), self.vocation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::items::{ItemAttribute, ItemConfig, ItemId};
    use crate::persistence::test_fixtures::a_test_snapshot;
    use std::collections::HashSet;

    fn a_knight_of_level(level: u16, slots: HashMap<InventorySlot, Item>) -> Agent {
        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.skills.insert(
            SkillType::Level,
            SkillValue {
                value: level,
                current_ticks: 0,
            },
        );
        snapshot.inventory = slots;
        Agent::from_player(snapshot)
    }

    fn item(flags: HashSet<ItemFlag>, attributes: Vec<ItemAttribute>) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1),
                "thing".to_string(),
                None,
                None,
                flags,
                attributes,
            )),
            1,
        )
    }

    fn a_level_35_sword() -> Item {
        item(
            HashSet::new(),
            vec![
                ItemAttribute::WeaponType(WeaponType::Sword),
                ItemAttribute::WeaponAttack(35),
                ItemAttribute::RequiredLevel(35),
            ],
        )
    }

    #[test]
    fn a_weapon_above_the_players_level_fights_as_an_empty_hand() {
        let below = a_knight_of_level(
            34,
            HashMap::from([(InventorySlot::LeftHand, a_level_35_sword())]),
        );
        let player = below.get_player().unwrap();
        assert!(player.weapon().is_none());
        assert_eq!(player.weapon_type(), WeaponType::None);
        assert_eq!(player.weapon_attack(), 5);

        let at = a_knight_of_level(
            35,
            HashMap::from([(InventorySlot::LeftHand, a_level_35_sword())]),
        );
        assert_eq!(at.get_player().unwrap().weapon_attack(), 35);
    }

    #[test]
    fn a_weapon_for_another_vocation_fights_as_an_empty_hand() {
        let bow = item(
            HashSet::new(),
            vec![
                ItemAttribute::WeaponType(WeaponType::Bow),
                ItemAttribute::Vocations([Vocation::Paladin].into_iter().collect()),
            ],
        );
        let knight = a_knight_of_level(100, HashMap::from([(InventorySlot::LeftHand, bow)]));

        assert_eq!(knight.get_player().unwrap().weapon_type(), WeaponType::None);
    }

    #[test]
    fn ammunition_the_player_cannot_use_is_passed_over() {
        let bow = item(
            HashSet::new(),
            vec![ItemAttribute::WeaponType(WeaponType::Bow)],
        );
        let flash_arrow = item(
            HashSet::new(),
            vec![
                ItemAttribute::AmmoType(AmmoType::Arrow),
                ItemAttribute::RequiredLevel(20),
            ],
        );
        let arrow = item(
            HashSet::new(),
            vec![ItemAttribute::AmmoType(AmmoType::Arrow)],
        );
        let arrow_guid = arrow.guid;
        let mut quiver = item(
            HashSet::from([ItemFlag::Container, ItemFlag::AmmoContainer]),
            vec![ItemAttribute::Capacity(2)],
        );
        quiver.content = Some(Box::new(vec![flash_arrow, arrow]));
        let knight = a_knight_of_level(
            1,
            HashMap::from([
                (InventorySlot::LeftHand, bow),
                (InventorySlot::RightHand, quiver),
            ]),
        );

        assert_eq!(
            knight.get_player().unwrap().weapon_ammo().map(|it| it.guid),
            Some(arrow_guid)
        );
    }

    #[test]
    fn a_quiver_the_player_cannot_use_supplies_nothing() {
        let bow = item(
            HashSet::new(),
            vec![ItemAttribute::WeaponType(WeaponType::Bow)],
        );
        let mut quiver = item(
            HashSet::from([ItemFlag::Container, ItemFlag::AmmoContainer]),
            vec![
                ItemAttribute::Capacity(1),
                ItemAttribute::RequiredLevel(150),
            ],
        );
        quiver.content = Some(Box::new(vec![item(
            HashSet::new(),
            vec![ItemAttribute::AmmoType(AmmoType::Arrow)],
        )]));
        let knight = a_knight_of_level(
            1,
            HashMap::from([
                (InventorySlot::LeftHand, bow),
                (InventorySlot::RightHand, quiver),
            ]),
        );

        assert!(knight.get_player().unwrap().weapon_ammo().is_none());
    }

    #[test]
    fn a_shield_above_the_players_level_is_not_a_shield() {
        let shield = item(
            HashSet::new(),
            vec![ItemAttribute::Defense(30), ItemAttribute::RequiredLevel(50)],
        );
        let knight = a_knight_of_level(49, HashMap::from([(InventorySlot::RightHand, shield)]));

        assert!(!knight.get_player().unwrap().has_shield());
    }
}
