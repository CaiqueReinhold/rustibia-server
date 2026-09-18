//! Fixtures shared across the crate's tests: the database ones below, used by the save
//! tests (`player.rs`) and the login tests (`login.rs`), and `a_spell`, which the cast-path
//! tests build a mutable `Spell` from.
//!
//! They were private to `player.rs` until login moved out of it. Sharing them rather than
//! duplicating matters because of what they encode: `accounts.id` and `players.id` are
//! `GENERATED ALWAYS`, so ids cannot be chosen and must be read back, and `save` is an
//! `UPDATE` — a character row has to exist before anything here can write to it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sqlx::PgPool;

use crate::entities::agent::{OutfitColors, OutfitId};
use crate::entities::creature::CreatureVoices;
use crate::entities::player::PlayerId;
use crate::entities::spells::{
    PowerCurve, Spell, SpellDelivery, SpellEffect, SpellGroup, SpellHealing, SpellId,
};
use crate::entities::targeting::TargetMode;
use crate::entities::vocation::Vocation;
use crate::entities::{
    Bounds,
    agent::{Agent, Facing, Pool},
    combat::CombatElement,
    creature::{BloodType, CreatureAttackDamage, CreatureKind},
    inventory::InventorySlot,
    items::{Item, ItemAttribute, ItemConfig, ItemFlag, ItemId},
    position::Position,
    skills::{SkillType, SkillValue},
};
use crate::game::TickDelta;
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

pub async fn insert_account(pool: &PgPool) -> i32 {
    sqlx::query_scalar::<_, i32>(
        "INSERT INTO accounts (email, password_hash) VALUES ($1, $2) RETURNING id",
    )
    .bind(format!("fixture-{}@example.com", uuid::Uuid::now_v7()))
    .bind("not-a-real-hash")
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Creates the character row. The game server can no longer do this itself — the website
/// owns character creation, and `save` only updates.
pub async fn insert_character(pool: &PgPool, account_id: i32) -> i32 {
    sqlx::query_scalar::<_, i32>(
        "INSERT INTO players \
         (account_id, name, vocation, sex, pos_x, pos_y, pos_z, origin_x, origin_y, origin_z, \
          facing, life_cur, life_max, mana_cur, mana_max, capacity, speed, \
          outfit_id, outfit_head, outfit_body, outfit_legs, outfit_feet) \
         VALUES ($1, $2, 0, 1, 1028, 1028, 7, 1028, 1028, 7, \
                 2, 150, 150, 0, 0, 400, 400, 133, 1, 2, 3, 4) \
         RETURNING id",
    )
    .bind(account_id)
    .bind(format!("Rizael{}", uuid::Uuid::now_v7().as_u128() % 100000))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A live game token for `character_id`, as the site would have minted.
pub async fn insert_token(pool: &PgPool, character_id: i32) -> String {
    insert_token_valid_for(pool, character_id, "1 hour").await
}

/// `interval` is Postgres interval syntax, and may be negative (`"-1 hour"`) for an
/// already-expired token.
///
/// Stores the digest and returns the plaintext, exactly as the site's mint path does —
/// inserting the plaintext here would make the load tests pass against a lookup that
/// production could never satisfy.
pub async fn insert_token_valid_for(pool: &PgPool, character_id: i32, interval: &str) -> String {
    let token = format!("token-{}", uuid::Uuid::now_v7());
    sqlx::query(&format!(
        "INSERT INTO game_tokens (token_hash, character_id, valid_until) \
         VALUES ($1, $2, NOW() + INTERVAL '{interval}')"
    ))
    .bind(super::login::hash_token_for_tests(&token))
    .bind(character_id)
    .execute(pool)
    .await
    .unwrap();
    token
}

pub async fn token_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM game_tokens")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The snapshot used by every save test. `id` and `account_id` are parameters because
/// both come from identity sequences rather than literals.
pub fn a_test_snapshot(id: u32, account_id: i32) -> PlayerSnapshot {
    PlayerSnapshot {
        id: PlayerId(id),
        account_id,
        admin: false,
        name: "Rizael".to_string(),
        vocation: Vocation::Knight,
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
/// `Item.content`. Must stay nested: a flat inventory does not exercise the recursive clone.
pub fn a_full_backpack() -> HashMap<InventorySlot, Item> {
    let coin = an_item_config(
        ItemId(2148),
        HashSet::from([ItemFlag::Take, ItemFlag::Cumulative]),
        Vec::from([ItemAttribute::Weight(1)]),
    );

    let mut backpack = a_container(ItemId(1988), 20);
    let outer = backpack.content.as_mut().unwrap();
    for pouch_id in 0..4u16 {
        let mut pouch = a_container(ItemId(1990 + pouch_id), 8);
        let inner = pouch.content.as_mut().unwrap();
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

/// The one `CreatureKind` every test builds on, so a new field on the struct is filled in
/// here and nowhere else. Callers override what their test is about with struct update
/// syntax: `CreatureKind { armor: 30, ..a_creature_kind("Dragon") }`.
pub fn a_creature_kind(name: &str) -> CreatureKind {
    CreatureKind {
        name: name.to_string(),
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
        say: CreatureVoices {
            cooldown: TickDelta(100),
            chance: 10000,
            sentences: vec!["sentence".to_owned()],
        },
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
