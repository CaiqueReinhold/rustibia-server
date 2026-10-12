//! Fixtures shared across the crate's tests, including `a_spell`, which the cast-path
//! tests build a mutable `Spell` from.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::entities::agent::{OutfitColors, OutfitId};
use crate::entities::npc::Npc;
use crate::entities::player::PlayerId;
use crate::entities::sex::Sex;
use crate::entities::spells::{
    PowerCurve, Spell, SpellDelivery, SpellEffect, SpellGroup, SpellHealing, SpellId,
};
use crate::entities::targeting::TargetMode;
use crate::entities::vocation::Vocation;
use crate::entities::voices::Voices;
use crate::entities::{
    Bounds,
    agent::{Agent, Facing, Pool},
    combat::CombatElement,
    creature::{BloodType, CreatureAttackDamage, CreatureFlag, CreatureKind},
    inventory::InventorySlot,
    items::{Item, ItemAttribute, ItemConfig, ItemFlag, ItemId},
    position::Position,
    skills::{SkillType, SkillValue},
};
use crate::game::TickDelta;
use crate::persistence::npcs::parse_npc;
use crate::persistence::player::PlayerSnapshot;

/// A `TargetMode::Caster` heal is the one effect that cannot fail to resolve, which is what
/// lets a caller use this as the success case of a cast.
pub fn a_spell(level: u16, magic_level: u16, vocations: Vec<Vocation>) -> Spell {
    Spell {
        id: SpellId(1),
        name: "Probe".to_owned(),
        words: "probe".to_owned(),
        group: SpellGroup::Attack,
        group_cooldown: None,
        cooldown: TickDelta(0),
        mana: 0,
        level,
        magic_level,
        delivery: SpellDelivery::Words,
        icon: 1,
        vocations,
        effect: SpellEffect::Healing(SpellHealing {
            target: TargetMode::Caster,
            power: PowerCurve {
                base_power: 1.0,
                level_factor: 0.0,
                magic_factor: 0.0,
                melee_factor: 0.0,
                spread_min: 0.0,
                spread_max: 0.0,
                flat: 0.0,
            },
        }),
    }
}

/// An empty item catalogue. Restoring an inventory needs one, and every test here starts
/// a character with nothing equipped.
pub fn no_items() -> Arc<HashMap<ItemId, Arc<ItemConfig>>> {
    Arc::new(HashMap::new())
}

/// What the site's redemption answers for character 7.
pub fn a_character_record_json() -> serde_json::Value {
    serde_json::json!({
        "id": 7,
        "account_id": 3,
        "admin": false,
        "name": "Rizael",
        "vocation": 0,
        "sex": 1,
        "position": { "x": 1028, "y": 1029, "z": 7 },
        "origin": { "x": 1028, "y": 1028, "z": 7 },
        "facing": 2,
        "life": { "current": 140, "maximum": 150 },
        "mana": { "current": 0, "maximum": 0 },
        "capacity": 400,
        "speed": 120,
        "outfit": { "id": 128, "head": 78, "body": 69, "legs": 58, "feet": 76 },
        "skills": [{ "skill_type": 1, "value": 220, "current_ticks": 0 }],
        "inventory": {},
        "bank_balance": 0
    })
}

pub fn a_test_snapshot(id: u32, account_id: i32) -> PlayerSnapshot {
    PlayerSnapshot {
        id: PlayerId(id),
        account_id,
        admin: false,
        name: "Rizael".to_string(),
        vocation: Vocation::Knight,
        sex: Sex::Male,
        position: Position {
            x: 1028,
            y: 1028,
            z: 7,
        },
        origin: Position {
            x: 1028,
            y: 1028,
            z: 7,
        },
        facing: Facing::South,
        life: Pool {
            current: 100,
            maximum: 100,
        },
        mana: Pool {
            current: 100,
            maximum: 100,
        },
        capacity: 40000,
        speed: 120,
        outfit: (OutfitId(133), OutfitColors::new(1, 2, 3, 4)),
        skills: HashMap::from([(
            SkillType::Level,
            SkillValue {
                value: 1,
                current_ticks: 0,
            },
        )]),
        inventory: HashMap::new(),
        bank_balance: 0,
    }
}

fn an_item_config(
    id: ItemId,
    flags: HashSet<ItemFlag>,
    attributes: Vec<ItemAttribute>,
) -> Arc<ItemConfig> {
    Arc::new(ItemConfig::new(
        id,
        format!("item {id}"),
        None,
        None,
        flags,
        attributes,
    ))
}

fn a_container(id: ItemId, capacity: u8) -> Item {
    Item::new(
        an_item_config(
            id,
            HashSet::from([ItemFlag::Container]),
            [ItemAttribute::Capacity(capacity), ItemAttribute::Weight(10)].to_vec(),
        ),
        1,
    )
}

/// A backpack holding four pouches of eight items each — 37 `Item`s over three levels of
/// `Item::content`. Must stay nested: a flat inventory does not exercise the recursive clone.
pub fn a_full_backpack() -> HashMap<InventorySlot, Item> {
    let coin = an_item_config(
        ItemId(2148),
        HashSet::from([ItemFlag::Take, ItemFlag::Cumulative]),
        Vec::from([ItemAttribute::Weight(1)]),
    );

    let mut backpack = a_container(ItemId(1988), 20);
    let outer = backpack.content_mut().unwrap();
    for pouch_id in 0..4u16 {
        let mut pouch = a_container(ItemId(1990 + pouch_id), 8);
        let inner = pouch.content_mut().unwrap();
        for n in 0..8u8 {
            inner.push(Item::new(Arc::clone(&coin), n + 1));
        }
        outer.push(pouch);
    }

    HashMap::from([(InventorySlot::Backpack, backpack)])
}

pub fn a_player_with_a_full_backpack(id: u32, account_id: i32) -> PlayerSnapshot {
    let mut snapshot = a_test_snapshot(id, account_id);
    snapshot.inventory = a_full_backpack();
    snapshot
}

/// Undefended on purpose. Armour that quietly swallows every small hit would turn tests
/// about something else green for the wrong reason; anything testing mitigation asks for
/// it by name. Worth no experience, for the same reason.
pub fn a_test_creature(name: &str, life: u32, damage: (u32, u32)) -> Agent {
    a_creature(name, life, damage, 0, 0, 0, None)
}

pub fn a_test_creature_with_defences(
    name: &str,
    life: u32,
    damage: (u32, u32),
    armor: u16,
    defense: u16,
) -> Agent {
    a_creature(name, life, damage, armor, defense, 0, None)
}

pub fn a_test_creature_worth(name: &str, life: u32, damage: (u32, u32), experience: u32) -> Agent {
    a_creature(name, life, damage, 0, 0, experience, None)
}

/// A creature that runs at or below `flee_threshold`. The default fixtures carry no
/// threshold at all, which is how "never flees" is spelled.
pub fn a_test_creature_that_flees(
    name: &str,
    life: u32,
    damage: (u32, u32),
    flee_threshold: u32,
) -> Agent {
    a_creature(name, life, damage, 0, 0, 0, Some(flee_threshold))
}

pub fn a_test_passive_creature(name: &str, life: u32) -> Agent {
    Agent::from_creature_kind(
        Arc::new(CreatureKind {
            life: Pool {
                current: life,
                maximum: life,
            },
            flags: vec![CreatureFlag::Passive],
            ..a_creature_kind(name)
        }),
        Position::new(1028, 128, 7),
    )
}

/// The one `CreatureKind` every test builds on, so a new field on the struct is filled in
/// here and nowhere else. Callers override what their test is about with struct update
/// syntax: `CreatureKind { armor: 30, ..a_creature_kind("Dragon") }`.
pub fn a_creature_kind(name: &str) -> CreatureKind {
    CreatureKind {
        name: name.to_string(),
        name_description: None,
        life: Pool {
            current: 1,
            maximum: 1,
        },
        outfit: (OutfitId(21), OutfitColors::new(0, 0, 0, 0)),
        speed: 100,
        melee: CreatureAttackDamage {
            element: CombatElement::Physical,
            value: Bounds { min: 1, max: 2 },
            condition: None,
        },
        abilities: vec![],
        blood_type: BloodType::Blood,
        armor: 0,
        defense: 0,
        experience: 0,
        corpse: ItemId(1),
        loot_table: vec![],
        flee_threshold: None,
        target_distance: 1,
        dance_chance: 0,
        say: Voices {
            cooldown: TickDelta(100),
            chance: 10000,
            sentences: vec!["sentence".to_owned()],
        },
        flags: vec![],
    }
}

fn a_creature(
    name: &str,
    life: u32,
    damage: (u32, u32),
    armor: u16,
    defense: u16,
    experience: u32,
    flee_threshold: Option<u32>,
) -> Agent {
    Agent::from_creature_kind(
        Arc::new(CreatureKind {
            life: Pool {
                current: life,
                maximum: life,
            },
            melee: CreatureAttackDamage {
                element: CombatElement::Physical,
                value: Bounds {
                    min: damage.0,
                    max: damage.1,
                },
                condition: None,
            },
            armor,
            defense,
            experience,
            flee_threshold,
            ..a_creature_kind(name)
        }),
        Position::new(1028, 128, 7),
    )
}

const AN_NPC: &str = r#"
name: Captain Jack
position: { x: 10, y: 10, z: 7 }
speed: 100
outfit: { id: 314, colors: [0, 0, 0, 0] }
walk_range: 3
talk_range: 3
say: { cooldown: 300, chance: 50000 }
arguments:
  DESTINATIONS:
    edron: { cost: 40, position: { x: 20, y: 20, z: 7 } }
  RING: 9999
shop:
  - { id: 9001, buy: 50, sell: 10 }
  - { id: 9002, buy: 3 }
  - { id: 9003, sell: 5 }
  - { id: 9003, fluid: milk, buy: 8 }
keywords:
  - words: GREET
    action: engage
    text: "Welcome, {{ PLAYERNAME }}."
  - words: GOODBYE
    action: dismiss
    text: Good bye.
  - words: ["trade"]
    action: trade
    text: "Take a look, {{ PLAYERNAME }}."
  - words: ["sail"]
    action: say
    text: Where to?
  - words: ["no"]
    action: say
    text: No what?
  - words: ["dice"]
    action: say
    text: "{{ uniform(1, 3) }}"
  - words: ["ferry"]
    condition: has_item(RING)
    action: say
    text: Ring?
    children:
      - words: ["yes"]
        action: teleport
        failed_text: No ring.
        params:
          cost: "0"
          item: RING
          position: DESTINATIONS.edron.position
  - words: ["ferry"]
    action: say
    text: No ring, no ferry.
  - words: ["edron"]
    action: say
    text: "Edron for {{ DESTINATIONS[WORD].cost }}?"
    children:
      - words: ["yes"]
        action: teleport
        failed_text: "No gold, {{ PLAYERNAME }}."
        params:
          cost: DESTINATIONS[PARENT_KEYWORD].cost
          position: DESTINATIONS[PARENT_KEYWORD].position
      - words: ["no"]
        action: say
        reset: true
        text: Maybe later.
  - words: ["count <n:int>"]
    action: say
    text: "{{ n }}?"
    children:
      - words: ["yes"]
        action: say
        text: "{{ n }} it is."
      - words: ["recount <n:int>"]
        action: say
        text: "{{ n }} then."
  - words: ["peek"]
    action: say
    text: "{{ n }}"
  - words: ["purse"]
    action: say
    set: { held: GOLD }
    text: "{{ held }}?"
    children:
      - words: ["yes"]
        action: say
        text: "{{ held }} it was."
  - words: ["balance"]
    action: say
    text: "{{ BALANCE }} banked."
  - words: ["deposit <amount:int>"]
    condition: amount > 0 and amount <= GOLD
    action: say
    text: "Deposit {{ amount }}?"
    children:
      - words: ["yes"]
        action: deposit
        text: "Deposited {{ amount }}."
        failed_text: Not enough gold.
        params: { amount: amount }
  - words: ["deposit <amount:int>"]
    action: say
    text: You do not have enough gold.
  - words: ["withdraw <amount:int>"]
    action: say
    text: "Withdraw {{ amount }}?"
    children:
      - words: ["yes"]
        action: withdraw
        text: "Withdrew {{ amount }}."
        failed_text: Not enough on your account.
        params: { amount: amount }
"#;

/// Stands at (10, 10, 7) and sails to (20, 20, 7) for 40, or for item 9999 when asked `ferry`
/// by a player carrying it. `no` answers at the root and, with a reset, under `edron`, so a test
/// can tell which level matched. `count <n:int>` carries `n` to its children and `peek` reads `n`
/// from the root, where it is undefined. `trade` opens a shop selling torches for 50 (bought back
/// for 10), arrows for 3, empty vials bought back for 5 and vials of milk sold for 8, from
/// `a_shop_catalogue`.
pub fn an_npc() -> Arc<Npc> {
    Arc::new(
        parse_npc(
            std::path::Path::new("an_npc.yaml"),
            AN_NPC,
            &a_shop_catalogue(),
        )
        .unwrap(),
    )
}

pub const A_TORCH: ItemId = ItemId(9001);
pub const AN_ARROW: ItemId = ItemId(9002);
pub const A_VIAL: ItemId = ItemId(9003);

/// A torch carried one per slot, an arrow that stacks and a vial that holds a fluid, for
/// `an_npc`'s shop.
pub fn a_shop_catalogue() -> HashMap<ItemId, Arc<ItemConfig>> {
    HashMap::from([
        (
            A_TORCH,
            Arc::new(ItemConfig::new(
                A_TORCH,
                "torch".to_string(),
                None,
                None,
                [ItemFlag::Take],
                [ItemAttribute::Weight(500)],
            )),
        ),
        (
            AN_ARROW,
            Arc::new(ItemConfig::new(
                AN_ARROW,
                "arrow".to_string(),
                None,
                None,
                [ItemFlag::Take, ItemFlag::Cumulative],
                [ItemAttribute::Weight(70)],
            )),
        ),
        (
            A_VIAL,
            Arc::new(ItemConfig::new(
                A_VIAL,
                "vial".to_string(),
                None,
                None,
                [ItemFlag::Take, ItemFlag::LiquidContainer],
                [ItemAttribute::Weight(180)],
            )),
        ),
    ])
}
