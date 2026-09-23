use std::{
    fmt::Display,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use smallvec::SmallVec;
use strum::{EnumCount, EnumIter};

use crate::{
    constants::items::MAX_STACK_AMOUNT,
    entities::{
        Bounds,
        agent::AgentKey,
        combat::{AmmoType, CombatElement, WeaponType},
        conditions::ConditionSpec,
        effects::{AreaShape, EffectId, MissileId},
        inventory::InventorySlot,
        position::{ItemPlacement, Position},
        spells::SpellId,
    },
    game::TickDelta,
    local_id::LocalId,
};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Hash)]
#[repr(transparent)]
pub struct ItemGuid(pub u64);

#[derive(
    Copy, Clone, Eq, PartialEq, Hash, Debug, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
#[repr(transparent)]
pub struct ItemId(pub u16);

impl Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
/// An open container as one player's session names it on the wire. Session-local and
/// reused — see `LocalIdMap`, which is the only thing that may mint one.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[repr(transparent)]
pub struct ContainerId(pub u16);

impl LocalId for ContainerId {
    fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    fn raw(self) -> u16 {
        self.0
    }
}

impl ItemGuid {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        ItemGuid(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for ItemGuid {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for ItemGuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, EnumCount, EnumIter)]
#[repr(u8)]
pub enum ItemFlag {
    Ground,
    Unmove,
    Unpass,
    Take,
    FullBank,
    Bottom,
    Cumulative,
    Container,
    Usable,
    Multiuse,
    Avoid,
    AmmoContainer,
    LiquidPool,
    Unreplaceable,
}

impl ItemFlag {
    pub const fn bit(self) -> u32 {
        1 << self as u32
    }
}

#[derive(Debug, Default, PartialEq, Eq, Hash, Clone, Copy)]
pub struct ItemFlags(u32);

impl ItemFlags {
    pub const fn new() -> Self {
        ItemFlags(0)
    }

    pub const fn with(self, flag: ItemFlag) -> Self {
        ItemFlags(self.0 | flag.bit())
    }

    pub const fn contains(self, flag: ItemFlag) -> bool {
        self.0 & flag.bit() != 0
    }
}

impl FromIterator<ItemFlag> for ItemFlags {
    fn from_iter<I: IntoIterator<Item = ItemFlag>>(iter: I) -> Self {
        iter.into_iter().fold(ItemFlags::new(), ItemFlags::with)
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum FluidType {
    None = 0,
    Water = 1,
    Mana = 2,
    Beer = 3,
    Oil = 4,
    Blood = 5,
    Slime = 6,
    Mud = 7,
    Lemonade = 8,
    Milk = 9,
    Wine = 10,
    Health = 11,
    Urine = 12,
    Rum = 13,
    FruitJuice = 14,
    CoconutMilk = 15,
    Tea = 16,
    Mead = 17,
    Ink = 18,
    Candy = 19,
    Chocolate = 20,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum FloorChangeDirection {
    Up,
    Down,
    North,
    East,
    South,
    West,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemAttribute {
    Capacity(u8),
    Weight(u32),
    FloorChange(FloorChangeDirection),
    Inventory(InventorySlot),
    TileFriction(u16),
    Action(ItemAction),
    MultiAction(ItemMultiAction),
    Decay {
        duration: TickDelta,
        decay_to: ItemId,
    },
    WeaponType(WeaponType),
    WeaponAttack(u16),
    WeaponElement(CombatElement),
    AmmoType(AmmoType),
    WeaponRange(u8),
    WeaponArea(Arc<AreaShape>, EffectId),
    HitChance(i16),
    MaxHitChance(u8),
    ManaCost(u32),
    MissileId(MissileId),
    Defense(u16),
    ExtraDef(i16),
    Armor(u16),
    Speed(i16),
    Field(Arc<ConditionSpec>),
}

#[derive(Debug)]
pub struct ItemConfig {
    pub id: ItemId,
    pub name: String,
    pub description: Option<String>,
    pub article: Option<String>,
    flags: ItemFlags,
    attributes: SmallVec<[ItemAttribute; 4]>,
}

macro_rules! attr_accessors {
    ($($name:ident -> $variant:ident: $ty:ty,)*) => {
        $(
            pub fn $name(&self) -> Option<$ty> {
                self.get_attributes().find_map(|attr| match attr {
                    ItemAttribute::$variant(a) => Some(*a),
                    _ => None,
                })
            }
        )*
    };
}

impl ItemConfig {
    pub fn new(
        id: ItemId,
        name: String,
        description: Option<String>,
        article: Option<String>,
        flags: impl IntoIterator<Item = ItemFlag>,
        attributes: impl IntoIterator<Item = ItemAttribute>,
    ) -> Self {
        ItemConfig {
            id,
            name,
            description,
            article,
            flags: flags.into_iter().collect(),
            attributes: SmallVec::from_iter(attributes),
        }
    }

    pub fn has_flag(&self, flag: ItemFlag) -> bool {
        self.flags.contains(flag)
    }

    fn get_attributes(&self) -> impl Iterator<Item = &ItemAttribute> {
        self.attributes.iter()
    }

    attr_accessors! {
        attr_capacity -> Capacity: u8,
        attr_weight -> Weight: u32,
        attr_floor_change -> FloorChange: FloorChangeDirection,
        attr_inventory -> Inventory: InventorySlot,
        attr_tile_friction -> TileFriction: u16,
        attr_action -> Action: ItemAction,
        attr_multi_action -> MultiAction: ItemMultiAction,
        attr_armor -> Armor: u16,
        attr_extra_def -> ExtraDef: i16,
        attr_defense -> Defense: u16,
        attr_weapon_type -> WeaponType: WeaponType,
        attr_weapon_attack -> WeaponAttack: u16,
        attr_weapon_element -> WeaponElement: CombatElement,
        attr_ammo_type -> AmmoType: AmmoType,
        attr_weapon_range -> WeaponRange: u8,
        attr_hit_chance -> HitChance: i16,
        attr_max_hit_chance -> MaxHitChance: u8,
        attr_mana_cost -> ManaCost: u32,
        attr_missile_id -> MissileId: MissileId,
        attr_speed -> Speed: i16,
    }

    pub fn attr_decay(&self) -> Option<(TickDelta, ItemId)> {
        self.get_attributes().find_map(|attr| match attr {
            ItemAttribute::Decay { decay_to, duration } => Some((*duration, *decay_to)),
            _ => None,
        })
    }

    pub fn attr_field(&self) -> Option<&Arc<ConditionSpec>> {
        self.get_attributes().find_map(|attr| match attr {
            ItemAttribute::Field(spec) => Some(spec),
            _ => None,
        })
    }

    pub fn attr_weapon_area(&self) -> Option<(&AreaShape, EffectId)> {
        self.get_attributes().find_map(|attr| match attr {
            ItemAttribute::WeaponArea(shape, effect_id) => Some((shape.as_ref(), *effect_id)),
            _ => None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub guid: ItemGuid,
    pub config: Arc<ItemConfig>,
    pub amount: u8,
    pub fluid: Option<FluidType>,
    /// Boxed because only a container ever has one, and an inline `Vec` was 24 bytes on
    /// every item — 17.7M of them sit inside the shipped map's tiles. The indirection is
    /// the point, so `box_collection`'s usual advice does not apply.
    #[allow(clippy::box_collection)]
    pub content: Option<Box<Vec<Item>>>,
    pub owner: Option<AgentKey>,
}

impl Item {
    pub fn new(config: Arc<ItemConfig>, amount: u8) -> Self {
        let content = if config.has_flag(ItemFlag::Container) {
            Some(Box::new(Vec::new()))
        } else {
            None
        };
        Item {
            config,
            guid: ItemGuid::new(),
            amount,
            fluid: None,
            content,
            owner: None,
        }
    }

    pub fn new_fluid(config: Arc<ItemConfig>, fluid: FluidType) -> Self {
        Item {
            config,
            guid: ItemGuid::new(),
            amount: 1,
            fluid: Some(fluid),
            content: None,
            owner: None,
        }
    }

    pub fn wire_subtype(&self) -> u8 {
        self.fluid.map(|f| f as u8).unwrap_or(self.amount)
    }

    pub fn get_name(&self) -> &str {
        &self.config.name
    }

    pub fn available_capacity(&self) -> Option<usize> {
        let cap = self.config.attr_capacity()? as usize;
        let used = self.content.as_ref().map(|c| c.len()).unwrap_or(0);
        Some(cap.saturating_sub(used))
    }

    pub fn total_weight(&self) -> u32 {
        let own = self.config.attr_weight().unwrap_or(0) * self.amount as u32;
        let inner = self
            .content
            .as_ref()
            .map_or(0, |items| items.iter().map(|i| i.total_weight()).sum());
        own + inner
    }

    pub fn find_by_guid(&self, guid: &ItemGuid) -> Option<&Item> {
        if self.guid == *guid {
            return Some(self);
        }
        self.content
            .as_ref()?
            .iter()
            .find_map(|i| i.find_by_guid(guid))
    }

    pub fn find_by_guid_mut(&mut self, guid: &ItemGuid) -> Option<&mut Item> {
        if self.guid == *guid {
            return Some(self);
        }
        self.content
            .as_mut()?
            .iter_mut()
            .find_map(|i| i.find_by_guid_mut(guid))
    }

    pub fn id(&self) -> ItemId {
        self.config.id
    }

    /// Splits `amount` off this stack into a new item with its own guid. The caller must have
    /// established `amount < self.amount`.
    pub fn split_off(&mut self, amount: u8) -> Item {
        self.amount -= amount;
        Item {
            guid: ItemGuid::new(),
            config: self.config.clone(),
            amount,
            fluid: None,
            content: None,
            owner: None,
        }
    }

    pub fn stacks_with(&self, other: &Item) -> bool {
        self.id() == other.id()
            && self.config.has_flag(ItemFlag::Cumulative)
            && self.amount < MAX_STACK_AMOUNT
    }

    pub fn top_up_from(&mut self, other: &mut Item) -> u8 {
        let moved = other.amount.min(MAX_STACK_AMOUNT - self.amount);
        self.amount += moved;
        other.amount -= moved;
        moved
    }

    /// Removes `amount` of `guid` from somewhere inside this container, reporting the removed
    /// item and the container it came out of. `None` when this item does not hold `guid`, or
    /// holds fewer than `amount` of it.
    pub fn remove_nested(
        &mut self,
        guid: &ItemGuid,
        amount: u8,
    ) -> Option<(Item, (ItemGuid, usize))> {
        let content = self.content.as_mut()?;

        if let Some(idx) = content.iter().position(|i| i.guid == *guid) {
            let held = content[idx].amount;
            return match held.cmp(&amount) {
                std::cmp::Ordering::Greater => {
                    Some((content[idx].split_off(amount), (self.guid, idx)))
                }
                std::cmp::Ordering::Equal => Some((content.remove(idx), (self.guid, idx))),
                std::cmp::Ordering::Less => None,
            };
        }

        content
            .iter_mut()
            .find_map(|item| item.remove_nested(guid, amount))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemRef {
    pub guid: ItemGuid,
    pub placement: ItemPlacement,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientItemRef {
    pub position: Position,
    pub item_id: ItemId,
    pub stack_index: u8,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Copy)]
pub enum ItemAction {
    Transform {
        into: ItemId,
    },
    Door {
        new: ItemId,
    },
    Food {
        duration: TickDelta,
        message_index: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ItemMultiAction {
    Shovel,
    Rope,
    Potion {
        health: Option<Bounds>,
        mana: Option<Bounds>,
        flask: Option<ItemId>,
    },
    Rune {
        spell: SpellId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::conditions::SpecSchedule;
    use std::collections::HashSet;
    use strum::IntoEnumIterator;

    fn a_stack(amount: u8) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(3031),
                "gold coin".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Cumulative, ItemFlag::Take]),
                Vec::new(),
            )),
            amount,
        )
    }

    /// A split that kept the source guid would leave two items answering to one identity, and
    /// every later lookup by guid could reach either.
    #[test]
    fn a_split_stack_gets_an_identity_of_its_own() {
        let mut stack = a_stack(50);
        let original = stack.guid;

        let taken = stack.split_off(20);

        assert_eq!(taken.amount, 20);
        assert_eq!(stack.amount, 30);
        assert_ne!(taken.guid, original);
        assert_eq!(stack.guid, original);
        assert_eq!(taken.id(), stack.id());
    }

    /// The client repeats this enum with the same discriminants, and nothing
    /// links the two -- they are separate repositories. This literal is the pin:
    /// the matching assertion lives in `rustibia-client/src/items/fluid.rs`, and
    /// if the two ever disagree every fluid in the game silently recolours.
    ///
    /// One sample is enough only because the discriminants are written
    /// explicitly rather than left positional, so reordering the variants cannot
    /// change any value.
    #[test]
    fn blood_is_five_on_the_wire() {
        assert_eq!(FluidType::Blood as u8, 5);
    }

    /// Two variants sharing a bit is the one failure a bitfield has that a `HashSet`
    /// does not, and it would not look like a bug: an item would simply answer `true`
    /// to a flag nobody gave it. Set each flag alone and check every other one.
    #[test]
    fn every_flag_owns_a_bit_of_its_own() {
        for set in ItemFlag::iter() {
            let flags = ItemFlags::new().with(set);
            for other in ItemFlag::iter() {
                assert_eq!(
                    flags.contains(other),
                    other == set,
                    "{set:?} answers for {other:?}"
                );
            }
        }
    }

    fn only(attr: ItemAttribute) -> ItemConfig {
        ItemConfig::new(
            ItemId(1),
            "thing".to_string(),
            None,
            None,
            [],
            Vec::from([attr]),
        )
    }

    fn answered(c: &ItemConfig) -> Vec<&'static str> {
        let mut found = Vec::new();
        let mut check = |name, present: bool| {
            if present {
                found.push(name);
            }
        };
        check("capacity", c.attr_capacity().is_some());
        check("weight", c.attr_weight().is_some());
        check("floor_change", c.attr_floor_change().is_some());
        check("inventory", c.attr_inventory().is_some());
        check("tile_friction", c.attr_tile_friction().is_some());
        check("action", c.attr_action().is_some());
        check("multi_action", c.attr_multi_action().is_some());
        check("armor", c.attr_armor().is_some());
        check("extra_def", c.attr_extra_def().is_some());
        check("defense", c.attr_defense().is_some());
        check("weapon_type", c.attr_weapon_type().is_some());
        check("weapon_attack", c.attr_weapon_attack().is_some());
        check("weapon_element", c.attr_weapon_element().is_some());
        check("ammo_type", c.attr_ammo_type().is_some());
        check("weapon_range", c.attr_weapon_range().is_some());
        check("hit_chance", c.attr_hit_chance().is_some());
        check("max_hit_chance", c.attr_max_hit_chance().is_some());
        check("mana_cost", c.attr_mana_cost().is_some());
        check("missile_id", c.attr_missile_id().is_some());
        check("speed", c.attr_speed().is_some());
        check("decay", c.attr_decay().is_some());
        check("field", c.attr_field().is_some());
        found
    }

    /// The accessors are generated from a `name -> Variant: Type` list, and a name paired with
    /// the wrong variant compiles whenever the two share a Rust type -- which four of them do
    /// for `u16`, three for `u8`, three for `i16` and two for `u32`. Nothing else would catch
    /// `attr_armor -> Defense`.
    #[test]
    fn an_attribute_is_read_by_exactly_one_accessor() {
        for (attr, expected) in [
            (ItemAttribute::Capacity(1), "capacity"),
            (ItemAttribute::Weight(1), "weight"),
            (
                ItemAttribute::FloorChange(FloorChangeDirection::Up),
                "floor_change",
            ),
            (ItemAttribute::Inventory(InventorySlot::Head), "inventory"),
            (ItemAttribute::TileFriction(1), "tile_friction"),
            (
                ItemAttribute::Action(ItemAction::Transform { into: ItemId(9) }),
                "action",
            ),
            (
                ItemAttribute::MultiAction(ItemMultiAction::Shovel),
                "multi_action",
            ),
            (ItemAttribute::Armor(1), "armor"),
            (ItemAttribute::ExtraDef(1), "extra_def"),
            (ItemAttribute::Defense(1), "defense"),
            (ItemAttribute::WeaponType(WeaponType::Axe), "weapon_type"),
            (ItemAttribute::WeaponAttack(1), "weapon_attack"),
            (
                ItemAttribute::WeaponElement(CombatElement::Fire),
                "weapon_element",
            ),
            (ItemAttribute::AmmoType(AmmoType::Arrow), "ammo_type"),
            (ItemAttribute::WeaponRange(1), "weapon_range"),
            (ItemAttribute::HitChance(1), "hit_chance"),
            (ItemAttribute::MaxHitChance(1), "max_hit_chance"),
            (ItemAttribute::ManaCost(1), "mana_cost"),
            (ItemAttribute::MissileId(MissileId(1)), "missile_id"),
            (ItemAttribute::Speed(1), "speed"),
            (
                ItemAttribute::Decay {
                    duration: TickDelta(1),
                    decay_to: ItemId(2),
                },
                "decay",
            ),
            (
                ItemAttribute::Field(Arc::new(ConditionSpec {
                    element: CombatElement::Fire,
                    damage: Bounds { min: 1, max: 1 },
                    interval: TickDelta(1),
                    schedule: SpecSchedule::Flat { count: 1 },
                    delayed: false,
                })),
                "field",
            ),
        ] {
            assert_eq!(answered(&only(attr)), vec![expected]);
        }
    }

    /// `ItemConfig::new` folds whatever it is handed, so the same flag twice must not
    /// mean anything different from the flag once -- and the catalogue hands it a
    /// `Vec<String>` straight out of YAML, which can repeat.
    #[test]
    fn a_repeated_flag_is_the_same_as_one() {
        let config = ItemConfig::new(
            ItemId(1),
            "thing".to_string(),
            None,
            None,
            [ItemFlag::Take, ItemFlag::Container, ItemFlag::Take],
            Vec::new(),
        );

        assert!(config.has_flag(ItemFlag::Take));
        assert!(config.has_flag(ItemFlag::Container));
        assert!(!config.has_flag(ItemFlag::Ground));
    }

    /// The wire byte is overloaded the way OT overloads it, but the struct is
    /// not: a fluid never masquerades as a stack count internally, so nothing
    /// that reasons about quantities can read one by accident.
    #[test]
    fn a_fluid_item_sends_its_fluid_where_a_stack_sends_its_count() {
        let config = Arc::new(ItemConfig::new(
            ItemId(2886),
            "pool".to_string(),
            None,
            None,
            HashSet::new(),
            Vec::new(),
        ));

        let mut pool = Item::new(Arc::clone(&config), 1);
        pool.fluid = Some(FluidType::Blood);
        let stack = Item::new(config, 37);

        assert_eq!(pool.wire_subtype(), 5, "the fluid, not the amount");
        assert_eq!(stack.wire_subtype(), 37, "the amount, as before");
    }
}
