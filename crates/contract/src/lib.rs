//! The types that cross the boundary between `rustibia-server` and `rustibia-site`.
//!
//! Both crates depend on this one; neither depends on the other. That is the whole
//! point: before this crate existed, the two processes agreed on the shape of the
//! `players` row only by convention, checked at runtime by a schema test. Now the
//! agreement is a type, and a mismatch is a compile error on whichever side is stale.
//!
//! These are **mirrors of the stored row shape**, not domain types. `Coords` is not
//! the server's `Position`, `SkillRow::skill_type` is an `i16` and not `SkillType`.
//! That is intentional — the server's domain types live in `crates/server` and the
//! site must never link them. Each side maps into its own types at the edge.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct RedeemRequest {
    pub auth_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CharacterRecord {
    pub id: i32,
    pub account_id: i32,
    pub admin: bool,
    pub name: String,
    pub vocation: i16,
    pub position: Coords,
    pub origin: Coords,
    pub facing: i16,
    pub life: PoolValue,
    pub mana: PoolValue,
    pub capacity: i32,
    pub speed: i32,
    pub outfit: Outfit,
    pub skills: Vec<SkillRow>,
    /// Keyed by inventory slot index, matching `players.inventory`'s JSONB shape.
    pub inventory: std::collections::HashMap<String, StoredItemRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coords {
    pub x: i32,
    pub y: i32,
    pub z: i16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolValue {
    pub current: i32,
    pub maximum: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outfit {
    pub id: i16,
    pub head: i16,
    pub body: i16,
    pub legs: i16,
    pub feet: i16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillRow {
    pub skill_type: i16,
    pub value: i16,
    pub current_ticks: i64,
}

/// One item in a stored inventory, possibly a container with `content`.
///
/// `content` and `action_id` are the optional fields in this crate, because the JSONB they
/// mirror omits them rather than writing `null`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredItemRecord {
    pub item_id: u16,
    pub amount: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<StoredItemRecord>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CharacterSave {
    pub id: i32,
    pub position: Coords,
    pub origin: Coords,
    pub facing: i16,
    pub life: PoolValue,
    pub mana: PoolValue,
    pub capacity: i32,
    pub speed: i32,
    pub outfit: Outfit,
    pub skills: Vec<SkillRow>,
    /// Keyed by inventory slot index, matching `players.inventory`'s JSONB shape.
    pub inventory: std::collections::HashMap<String, StoredItemRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldSave {
    pub tick: u64,
    pub characters: Vec<CharacterSave>,
    pub chunks: Vec<ChunkRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkRow {
    pub cx: i32,
    pub cy: i32,
    pub z: i16,
    pub tiles: Vec<TileRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TileRow {
    /// `ly * 16 + lx` within the chunk; the 16 is `CHUNK_SIDE` in the game server's `map.rs`.
    pub index: u16,
    pub items: Vec<PlacedItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacedItem {
    pub stack_index: u16,
    pub item: StoredItemRecord,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldSaveResult {
    pub skipped: Vec<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn a_record() -> CharacterRecord {
        CharacterRecord {
            id: 7,
            account_id: 3,
            admin: false,
            name: "Rizael".to_string(),
            vocation: 3,
            position: Coords {
                x: 1028,
                y: 1029,
                z: 7,
            },
            origin: Coords {
                x: 1028,
                y: 1028,
                z: 7,
            },
            facing: 2,
            life: PoolValue {
                current: 140,
                maximum: 150,
            },
            mana: PoolValue {
                current: 0,
                maximum: 0,
            },
            capacity: 400,
            speed: 100,
            outfit: Outfit {
                id: 128,
                head: 78,
                body: 69,
                legs: 58,
                feet: 76,
            },
            skills: vec![
                SkillRow {
                    skill_type: 0,
                    value: 1,
                    current_ticks: 0,
                },
                SkillRow {
                    skill_type: 1,
                    value: 220,
                    current_ticks: 0,
                },
            ],
            inventory: HashMap::from([(
                "5".to_string(),
                StoredItemRecord {
                    item_id: 2148,
                    amount: 1,
                    content: Some(vec![StoredItemRecord {
                        item_id: 2360,
                        amount: 10,
                        content: None,
                        action_id: None,
                    }]),
                    action_id: None,
                },
            )]),
        }
    }

    #[test]
    fn character_record_round_trips() {
        let json = serde_json::to_string(&a_record()).unwrap();
        let back: CharacterRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, 7);
        assert_eq!(back.name, "Rizael");
        assert_eq!(back.position.x, 1028);
        assert_eq!(back.position.y, 1029);
        assert_eq!(back.facing, 2);
        assert_eq!(back.life.current, 140);
        assert_eq!(back.outfit.id, 128);
        assert_eq!(back.skills.len(), 2);
        assert_eq!(back.skills[1].value, 220);
        assert_eq!(back.inventory["5"].item_id, 2148);
        assert_eq!(
            back.inventory["5"].content.as_ref().unwrap()[0].item_id,
            2360
        );
    }

    #[test]
    fn a_missing_field_is_an_error_and_not_a_default() {
        let json = serde_json::to_value(a_record()).unwrap();
        let mut object = json.as_object().unwrap().clone();
        object.remove("facing");

        let result = serde_json::from_value::<CharacterRecord>(serde_json::Value::Object(object));

        let err = result.expect_err("a record missing `facing` must not deserialize");
        assert!(
            err.to_string().contains("facing"),
            "the error must name the missing field, got: {err}"
        );
    }

    /// One case per field would be noise; this proves the property holds for every
    /// field rather than just the one above.
    #[test]
    fn no_field_of_character_record_is_optional() {
        let json = serde_json::to_value(a_record()).unwrap();
        let object = json.as_object().unwrap();

        for field in object.keys() {
            let mut without = object.clone();
            without.remove(field);
            assert!(
                serde_json::from_value::<CharacterRecord>(serde_json::Value::Object(without))
                    .is_err(),
                "`{field}` deserialized to a default when absent; every field of \
                 CharacterRecord must be required"
            );
        }
    }

    #[test]
    fn stored_item_omits_content_when_absent() {
        let json = serde_json::to_string(&StoredItemRecord {
            item_id: 2360,
            amount: 5,
            content: None,
            action_id: None,
        })
        .unwrap();

        assert!(
            !json.contains("content"),
            "content must be omitted, not null: {json}"
        );
    }

    /// The server writes this JSONB and the site reads it back out of the column, so
    /// the shape has to survive a trip through the column's own encoding unchanged.
    #[test]
    fn stored_item_accepts_a_body_written_without_content() {
        let item: StoredItemRecord =
            serde_json::from_str(r#"{"item_id":2360,"amount":5}"#).unwrap();

        assert_eq!(
            item,
            StoredItemRecord {
                item_id: 2360,
                amount: 5,
                content: None,
                action_id: None,
            }
        );
    }

    #[test]
    fn stored_item_omits_action_id_when_absent() {
        let json = serde_json::to_string(&StoredItemRecord {
            item_id: 2970,
            amount: 1,
            content: None,
            action_id: None,
        })
        .unwrap();

        assert!(
            !json.contains("action_id"),
            "action_id must be omitted: {json}"
        );
    }

    #[test]
    fn stored_item_written_before_action_ids_still_reads() {
        let item: StoredItemRecord =
            serde_json::from_str(r#"{"item_id":2970,"amount":1}"#).unwrap();

        assert_eq!(item.action_id, None);
    }

    #[test]
    fn redeem_request_round_trips() {
        let json = serde_json::to_string(&RedeemRequest {
            auth_token: "abc".to_string(),
        })
        .unwrap();
        let back: RedeemRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(back.auth_token, "abc");
    }
}
