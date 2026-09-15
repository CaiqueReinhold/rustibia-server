use std::collections::HashMap;
use std::sync::Arc;

use crate::entities::{
    combat::{AmmoType, CombatElement, WeaponType},
    effects::{AreaShape, EffectId},
    inventory::{Inventory, InventorySlot},
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
        inventory: Inventory,
        skills: HashMap<SkillType, SkillValue>,
    ) -> Self {
        Self {
            id,
            name,
            account_id,
            admin,
            last_logout_position,
            vocation,
            capacity,
            inventory: Arc::new(inventory),
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
            .map(|it| it.config.attr_defense().is_some())
            .unwrap_or(false)
    }

    pub fn weapon(&self) -> Option<&Item> {
        self.inventory.get(&InventorySlot::LeftHand)
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
                .filter(|it| it.config.has_flag(ItemFlag::AmmoContainer))
                .and_then(|quiv| {
                    quiv.content.as_ref().and_then(|content| {
                        content.iter().find(|it| {
                            it.config.attr_ammo_type().is_some_and(|at| {
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
}
