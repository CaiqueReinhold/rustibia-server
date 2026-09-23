use std::collections::HashMap;

use anyhow::Result;
use rustibia_contract::StoredItemRecord;
use rustibia_server::constants::items::MAX_STACK_AMOUNT;
use rustibia_server::entities::inventory::InventorySlot;
use rustibia_server::entities::skills::SkillType;
use sqlx::PgPool;

use crate::account::rest::CharacterId;
use crate::config::{CharacterKit, Inventory, KitConfig};

pub fn build_inventory(kit: &Inventory) -> HashMap<String, StoredItemRecord> {
    let content = kit
        .contents
        .iter()
        .flat_map(|stack| {
            (0..stack.stacks).map(|_| StoredItemRecord {
                item_id: stack.item,
                amount: stack.amount.min(MAX_STACK_AMOUNT),
                content: None,
            })
        })
        .collect();

    HashMap::from([
        (
            InventorySlot::Backpack.as_id().to_string(),
            StoredItemRecord {
                item_id: kit.backpack,
                amount: 1,
                content: Some(content),
            },
        ),
        (
            InventorySlot::RightHand.as_id().to_string(),
            StoredItemRecord {
                item_id: kit.weapon,
                amount: 1,
                content: None,
            },
        ),
    ])
}

/// Refuses an online character in the same statement that stocks it: the world holds
/// its state in memory and would overwrite this row at its next periodic save.
pub async fn stock(pool: &PgPool, character_id: CharacterId, kit: &KitConfig) -> Result<()> {
    let character = &kit.character;
    let inventory = serde_json::to_value(build_inventory(&kit.kit))?;
    let mut tx = pool.begin().await?;

    let updated = sqlx::query(
        "UPDATE players SET \
            pos_x = $2, pos_y = $3, pos_z = $4, \
            origin_x = $2, origin_y = $3, origin_z = $4, \
            life_cur = $5, life_max = $5, mana_cur = $6, mana_max = $6, \
            capacity = $7, inventory = $8 \
         WHERE id = $1 \
           AND NOT EXISTS (SELECT 1 FROM online_players WHERE character_id = $1)",
    )
    .bind(character_id.0)
    .bind(character.start.x)
    .bind(character.start.y)
    .bind(character.start.z)
    .bind(character.life)
    .bind(character.mana)
    .bind(character.capacity)
    .bind(&inventory)
    .execute(&mut *tx)
    .await?;

    if updated.rows_affected() == 0 {
        anyhow::bail!("character {} is online or does not exist", character_id.0);
    }

    for (skill_id, value) in skill_rows(character) {
        sqlx::query(
            "INSERT INTO player_skills (player_id, skill_type, value, current_ticks) \
             VALUES ($1, $2, $3, 0) \
             ON CONFLICT (player_id, skill_type) DO UPDATE \
                SET value = EXCLUDED.value, current_ticks = 0",
        )
        .bind(character_id.0)
        .bind(skill_id)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

fn skill_rows(character: &CharacterKit) -> Vec<(i16, i16)> {
    let mut rows = vec![(SkillType::Level.as_id() as i16, character.level)];
    rows.extend(
        character
            .skills
            .iter()
            .map(|(skill, value)| (skill.as_id() as i16, *value)),
    );
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Stack;

    fn a_kit() -> Inventory {
        Inventory {
            backpack: 2854,
            weapon: 3264,
            contents: vec![Stack {
                item: 266,
                stacks: 2,
                amount: 100,
            }],
        }
    }

    #[test]
    fn the_backpack_carries_the_stacks_and_the_weapon_stands_alone() {
        let inventory = build_inventory(&a_kit());

        let backpack = &inventory[&InventorySlot::Backpack.as_id().to_string()];
        assert_eq!(backpack.item_id, 2854);
        let content = backpack.content.as_ref().expect("a backpack holds content");
        assert_eq!(content.len(), 2);
        assert!(content.iter().all(|s| s.item_id == 266 && s.amount == 100));

        let weapon = &inventory[&InventorySlot::RightHand.as_id().to_string()];
        assert_eq!(weapon.item_id, 3264);
        assert_eq!(weapon.content, None);
    }

    #[test]
    fn a_stack_is_capped_at_the_protocol_maximum() {
        let kit = Inventory {
            contents: vec![Stack {
                item: 266,
                stacks: 1,
                amount: 255,
            }],
            ..a_kit()
        };

        let inventory = build_inventory(&kit);

        assert_eq!(
            inventory[&InventorySlot::Backpack.as_id().to_string()]
                .content
                .as_ref()
                .unwrap()[0]
                .amount,
            100
        );
    }
}
