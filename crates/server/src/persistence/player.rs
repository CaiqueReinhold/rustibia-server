//! Writing a player back to the database.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

use crate::entities::vocation::Vocation;
use crate::entities::{
    agent::{Facing, OutfitColors, OutfitId, Pool},
    inventory::InventorySlot,
    items::{Item, ItemId},
    player::PlayerId,
    position::Position,
    skills::{SkillType, SkillValue},
};

#[derive(Error, Debug)]
pub enum PlayerRepositoryError {
    #[error("Player not found")]
    NotFound,
    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub id: PlayerId,
    pub account_id: i32,
    pub admin: bool,
    pub position: Position,
    pub origin: Position,
    pub facing: Facing,
    pub name: String,
    pub vocation: Vocation,
    pub life: Pool,
    pub mana: Pool,
    pub capacity: u32,
    pub speed: u16,
    pub outfit: (OutfitId, OutfitColors),
    pub skills: HashMap<SkillType, SkillValue>,
    pub inventory: HashMap<InventorySlot, Item>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct StoredItem {
    item_id: ItemId,
    amount: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<Vec<StoredItem>>,
}

pub struct PlayerRepository {
    pool: PgPool,
}

impl PlayerRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn save(&self, snapshot: &PlayerSnapshot) -> Result<(), PlayerRepositoryError> {
        let inventory = serialize_inventory(&snapshot.inventory);
        let facing = snapshot.facing.as_id() as i16;
        let (outfit_id, colors) = snapshot.outfit;

        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(
            "UPDATE players SET \
             pos_x = $2, pos_y = $3, pos_z = $4, \
             origin_x = $5, origin_y = $6, origin_z = $7, \
             facing = $8, \
             life_cur = $9, life_max = $10, mana_cur = $11, mana_max = $12, \
             capacity = $13, speed = $14, \
             outfit_id = $15, outfit_head = $16, outfit_body = $17, \
             outfit_legs = $18, outfit_feet = $19, \
             inventory = $20 \
             WHERE id = $1",
        )
        .bind(snapshot.id.0 as i32)
        .bind(snapshot.position.x as i32)
        .bind(snapshot.position.y as i32)
        .bind(snapshot.position.z as i16)
        .bind(snapshot.origin.x as i32)
        .bind(snapshot.origin.y as i32)
        .bind(snapshot.origin.z as i16)
        .bind(facing)
        .bind(snapshot.life.current as i32)
        .bind(snapshot.life.maximum as i32)
        .bind(snapshot.mana.current as i32)
        .bind(snapshot.mana.maximum as i32)
        .bind(snapshot.capacity as i32)
        .bind(snapshot.speed as i32)
        .bind(outfit_id.0 as i16)
        .bind(colors.head as i16)
        .bind(colors.body as i16)
        .bind(colors.legs as i16)
        .bind(colors.feet as i16)
        .bind(sqlx::types::Json(&inventory))
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            return Err(PlayerRepositoryError::NotFound);
        }

        sqlx::query("DELETE FROM player_skills WHERE player_id = $1")
            .bind(snapshot.id.0 as i32)
            .execute(&mut *tx)
            .await?;

        for (skill_type, skill_value) in &snapshot.skills {
            sqlx::query(
                "INSERT INTO player_skills (player_id, skill_type, value, current_ticks) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(snapshot.id.0 as i32)
            .bind(skill_type.as_id() as i16)
            .bind(skill_value.value as i16)
            .bind(skill_value.current_ticks as i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }
}

fn serialize_inventory(inventory: &HashMap<InventorySlot, Item>) -> HashMap<String, StoredItem> {
    inventory
        .iter()
        .map(|(slot, item)| (slot.as_id().to_string(), serialize_item(item)))
        .collect()
}

fn serialize_item(item: &Item) -> StoredItem {
    StoredItem {
        item_id: item.id(),
        amount: item.amount,
        content: item
            .content
            .as_ref()
            .map(|children| children.iter().map(serialize_item).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::items::Item;
    use crate::persistence::test_fixtures::{a_test_snapshot, insert_account, insert_character};

    #[test]
    fn stored_item_omits_content_when_none() {
        let item = StoredItem {
            item_id: ItemId(2360),
            amount: 5,
            content: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(
            !json.contains("content"),
            "content field should be absent: {json}"
        );
    }

    #[test]
    fn stored_item_roundtrips_with_nested_content() {
        let item = StoredItem {
            item_id: ItemId(2148),
            amount: 1,
            content: Some(vec![StoredItem {
                item_id: ItemId(2360),
                amount: 10,
                content: None,
            }]),
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: StoredItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back.item_id, ItemId(2148));
        assert_eq!(back.amount, 1);
        let children = back.content.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].item_id, ItemId(2360));
        assert_eq!(children[0].amount, 10);
    }

    /// The site reads this column back through `rustibia_contract::StoredItemRecord`, so
    /// what `save` writes has to satisfy that type. Asserting it here is the compile-time
    /// half of the agreement the contract crate exists to enforce.
    #[test]
    fn what_save_writes_deserializes_as_the_contract_type() {
        let json = serde_json::to_string(&StoredItem {
            item_id: ItemId(2148),
            amount: 1,
            content: Some(vec![StoredItem {
                item_id: ItemId(2360),
                amount: 10,
                content: None,
            }]),
        })
        .unwrap();

        let record: rustibia_contract::StoredItemRecord = serde_json::from_str(&json)
            .expect("the site must be able to read what this module writes");

        assert_eq!(record.item_id, 2148);
        assert_eq!(record.content.unwrap()[0].item_id, 2360);
    }

    #[test]
    fn serialize_inventory_uses_slot_id_as_key() {
        use crate::entities::items::ItemConfig;
        use std::collections::HashSet;
        use std::sync::Arc;

        let config = Arc::new(ItemConfig::new(
            ItemId(2360),
            "sword".to_string(),
            None,
            None,
            HashSet::new(),
            Vec::new(),
        ));
        let item = Item::new(config, 1);
        let mut inv: HashMap<InventorySlot, Item> = HashMap::new();
        inv.insert(InventorySlot::RightHand, item);

        let stored = serialize_inventory(&inv);
        // RightHand::as_id() == 5
        assert!(
            stored.contains_key("5"),
            "expected key '5', got: {stored:?}"
        );
        assert_eq!(stored["5"].item_id, ItemId(2360));
        assert_eq!(stored["5"].amount, 1);
    }

    #[sqlx::test(migrations = "../site/migrations")]
    async fn save_returns_not_found_when_the_character_does_not_exist(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let repo = PlayerRepository::new(pool);

        let result = repo.save(&a_test_snapshot(999_999, account_id)).await;

        assert!(
            matches!(result, Err(PlayerRepositoryError::NotFound)),
            "saving an absent character must not silently succeed, got {result:?}"
        );
    }

    /// Skills are deleted and reinserted on every save, so a save that drops one has to
    /// leave the table consistent rather than half-written.
    #[sqlx::test(migrations = "../site/migrations")]
    async fn save_replaces_the_skill_rows_rather_than_accumulating_them(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let character_id = insert_character(&pool, account_id).await;
        let repo = PlayerRepository::new(pool.clone());

        let snapshot = a_test_snapshot(character_id as u32, account_id);
        repo.save(&snapshot).await.unwrap();
        repo.save(&snapshot).await.unwrap();

        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM player_skills WHERE player_id = $1")
                .bind(character_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(
            count,
            snapshot.skills.len() as i64,
            "two saves of the same skills must leave one row each, not two"
        );
    }
}
