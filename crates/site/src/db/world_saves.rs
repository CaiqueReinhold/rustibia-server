//! Writing a whole-world snapshot, and reading back the map chunks it saved.

use rustibia_contract::{CharacterSave, ChunkRow, TileRow, WorldSave, WorldSaveResult};
use sqlx::{PgConnection, PgPool, types::Json};

pub async fn apply(pool: &PgPool, save: &WorldSave) -> Result<WorldSaveResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut skipped = Vec::new();
    for character in &save.characters {
        if !write_character(&mut tx, character).await? {
            skipped.push(character.id);
        }
    }
    for chunk in &save.chunks {
        sqlx::query(
            "INSERT INTO map_chunks (cx, cy, z, tiles) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (cx, cy, z) DO UPDATE SET tiles = EXCLUDED.tiles",
        )
        .bind(chunk.cx)
        .bind(chunk.cy)
        .bind(chunk.z)
        .bind(Json(&chunk.tiles))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(WorldSaveResult { skipped })
}

pub async fn load_chunks(pool: &PgPool) -> Result<Vec<ChunkRow>, sqlx::Error> {
    let rows: Vec<(i32, i32, i16, Json<Vec<TileRow>>)> =
        sqlx::query_as("SELECT cx, cy, z, tiles FROM map_chunks")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(cx, cy, z, Json(tiles))| ChunkRow { cx, cy, z, tiles })
        .collect())
}

async fn write_character(
    conn: &mut PgConnection,
    save: &CharacterSave,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE players SET \
         pos_x = $2, pos_y = $3, pos_z = $4, \
         origin_x = $5, origin_y = $6, origin_z = $7, \
         facing = $8, \
         life_cur = $9, life_max = $10, mana_cur = $11, mana_max = $12, \
         capacity = $13, speed = $14, \
         outfit_id = $15, outfit_head = $16, outfit_body = $17, \
         outfit_legs = $18, outfit_feet = $19, \
         inventory = $20 \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(save.id)
    .bind(save.position.x)
    .bind(save.position.y)
    .bind(save.position.z)
    .bind(save.origin.x)
    .bind(save.origin.y)
    .bind(save.origin.z)
    .bind(save.facing)
    .bind(save.life.current)
    .bind(save.life.maximum)
    .bind(save.mana.current)
    .bind(save.mana.maximum)
    .bind(save.capacity)
    .bind(save.speed)
    .bind(save.outfit.id)
    .bind(save.outfit.head)
    .bind(save.outfit.body)
    .bind(save.outfit.legs)
    .bind(save.outfit.feet)
    .bind(Json(&save.inventory))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }

    sqlx::query("DELETE FROM player_skills WHERE player_id = $1")
        .bind(save.id)
        .execute(&mut *conn)
        .await?;
    for skill in &save.skills {
        sqlx::query(
            "INSERT INTO player_skills (player_id, skill_type, value, current_ticks) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(save.id)
        .bind(skill.skill_type)
        .bind(skill.value)
        .bind(skill.current_ticks)
        .execute(&mut *conn)
        .await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rustibia_contract::{Coords, Outfit, PlacedItem, PoolValue, SkillRow, StoredItemRecord};

    use super::*;
    use crate::{
        config::SiteConfig,
        db::{accounts::create_account, characters},
        domain::{sex::Sex, vocation::Vocation},
    };

    async fn a_character(pool: &PgPool, email: &str, name: &str) -> i32 {
        let account_id = create_account(pool, email, "hunter2hunter2")
            .await
            .unwrap()
            .id;
        let template = SiteConfig::load("config.yaml").unwrap().new_character;
        characters::create(
            pool,
            account_id,
            name,
            Vocation::Paladin,
            Sex::Male,
            &template,
        )
        .await
        .unwrap()
    }

    fn a_character_save(id: i32) -> CharacterSave {
        CharacterSave {
            id,
            position: Coords {
                x: 1030,
                y: 1031,
                z: 7,
            },
            origin: Coords {
                x: 1028,
                y: 1028,
                z: 7,
            },
            facing: 1,
            life: PoolValue {
                current: 37,
                maximum: 150,
            },
            mana: PoolValue {
                current: 5,
                maximum: 20,
            },
            capacity: 390,
            speed: 220,
            outfit: Outfit {
                id: 128,
                head: 1,
                body: 2,
                legs: 3,
                feet: 4,
            },
            skills: vec![SkillRow {
                skill_type: 0,
                value: 8,
                current_ticks: 1234,
            }],
            inventory: HashMap::new(),
        }
    }

    fn a_chunk(cx: i32, item_id: u16) -> ChunkRow {
        ChunkRow {
            cx,
            cy: 10,
            z: 7,
            tiles: vec![TileRow {
                index: 17,
                items: vec![PlacedItem {
                    stack_index: 2,
                    item: StoredItemRecord {
                        item_id,
                        amount: 3,
                        content: None,
                        action_id: None,
                    },
                }],
            }],
        }
    }

    async fn life_of(pool: &PgPool, id: i32) -> i32 {
        sqlx::query_scalar("SELECT life_cur FROM players WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_world_save_writes_characters_and_chunks(pool: PgPool) {
        let id = a_character(&pool, "a@example.com", "Rizael").await;
        let save = WorldSave {
            tick: 40,
            characters: vec![a_character_save(id)],
            chunks: vec![a_chunk(5, 3031)],
        };

        let result = apply(&pool, &save).await.unwrap();

        assert_eq!(
            result,
            WorldSaveResult {
                skipped: Vec::new()
            }
        );
        assert_eq!(life_of(&pool, id).await, 37);
        assert_eq!(load_chunks(&pool).await.unwrap(), vec![a_chunk(5, 3031)]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_chunk_saved_again_replaces_its_row(pool: PgPool) {
        apply(
            &pool,
            &WorldSave {
                tick: 1,
                characters: Vec::new(),
                chunks: vec![a_chunk(5, 3031)],
            },
        )
        .await
        .unwrap();

        apply(
            &pool,
            &WorldSave {
                tick: 2,
                characters: Vec::new(),
                chunks: vec![a_chunk(5, 3035)],
            },
        )
        .await
        .unwrap();

        assert_eq!(load_chunks(&pool).await.unwrap(), vec![a_chunk(5, 3035)]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_missing_or_deleted_character_is_skipped(pool: PgPool) {
        let deleted = a_character(&pool, "a@example.com", "Rizael").await;
        sqlx::query("UPDATE players SET deleted_at = NOW() WHERE id = $1")
            .bind(deleted)
            .execute(&pool)
            .await
            .unwrap();
        let save = WorldSave {
            tick: 1,
            characters: vec![a_character_save(deleted), a_character_save(999_999)],
            chunks: vec![a_chunk(5, 3031)],
        };

        let result = apply(&pool, &save).await.unwrap();

        assert_eq!(
            result,
            WorldSaveResult {
                skipped: vec![deleted, 999_999]
            }
        );
        assert_eq!(load_chunks(&pool).await.unwrap().len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_failure_anywhere_rolls_the_whole_save_back(pool: PgPool) {
        let first = a_character(&pool, "a@example.com", "Rizael").await;
        let second = a_character(&pool, "b@example.com", "Anaia").await;
        let mut duplicate_skill = a_character_save(second);
        duplicate_skill.skills.push(SkillRow {
            skill_type: 0,
            value: 9,
            current_ticks: 0,
        });
        let save = WorldSave {
            tick: 1,
            characters: vec![a_character_save(first), duplicate_skill],
            chunks: vec![a_chunk(5, 3031)],
        };

        assert!(apply(&pool, &save).await.is_err());

        assert_ne!(life_of(&pool, first).await, 37);
        assert!(load_chunks(&pool).await.unwrap().is_empty());
    }
}
