//! Writing the character state the game server sends.

use rustibia_contract::{CharacterSave, SaveOutcome, SaveResult, SaveResults};
use sqlx::{PgConnection, PgPool};

pub async fn apply(pool: &PgPool, characters: &[CharacterSave]) -> Result<SaveResults, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut results = Vec::with_capacity(characters.len());
    for save in characters {
        let outcome = apply_one(&mut tx, save).await?;
        results.push(SaveResult { id: save.id, outcome });
    }
    tx.commit().await?;
    Ok(SaveResults { results })
}

async fn apply_one(conn: &mut PgConnection, save: &CharacterSave) -> Result<SaveOutcome, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE players SET \
         pos_x = $2, pos_y = $3, pos_z = $4, \
         origin_x = $5, origin_y = $6, origin_z = $7, \
         facing = $8, \
         life_cur = $9, life_max = $10, mana_cur = $11, mana_max = $12, \
         capacity = $13, speed = $14, \
         outfit_id = $15, outfit_head = $16, outfit_body = $17, \
         outfit_legs = $18, outfit_feet = $19, \
         inventory = $20, save_version = $21 \
         WHERE id = $1 AND deleted_at IS NULL AND save_version < $21",
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
    .bind(sqlx::types::Json(&save.inventory))
    .bind(save.save_version)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    if updated == 0 {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM players WHERE id = $1 AND deleted_at IS NULL)",
        )
        .bind(save.id)
        .fetch_one(&mut *conn)
        .await?;
        return Ok(if exists { SaveOutcome::Stale } else { SaveOutcome::Gone });
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
    Ok(SaveOutcome::Applied)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rustibia_contract::{Coords, Outfit, PoolValue, SkillRow, StoredItemRecord};

    use super::*;
    use crate::{
        config::SiteConfig,
        db::{accounts::create_account, characters},
        domain::{sex::Sex, vocation::Vocation},
    };

    async fn a_character(pool: &PgPool, email: &str, name: &str) -> i32 {
        let account_id = create_account(pool, email, "hunter2hunter2").await.unwrap().id;
        let template = SiteConfig::load("config.yaml").unwrap().new_character;
        characters::create(pool, account_id, name, Vocation::Paladin, Sex::Male, &template)
            .await
            .unwrap()
    }

    fn a_save(id: i32, save_version: i64) -> CharacterSave {
        CharacterSave {
            id,
            save_version,
            position: Coords { x: 1030, y: 1031, z: 7 },
            origin: Coords { x: 1028, y: 1028, z: 7 },
            facing: 1,
            life: PoolValue { current: 37, maximum: 150 },
            mana: PoolValue { current: 5, maximum: 20 },
            capacity: 390,
            speed: 220,
            outfit: Outfit { id: 128, head: 1, body: 2, legs: 3, feet: 4 },
            skills: vec![SkillRow { skill_type: 0, value: 8, current_ticks: 1234 }],
            inventory: HashMap::from([(
                "5".to_string(),
                StoredItemRecord { item_id: 3031, amount: 12, content: None },
            )]),
        }
    }

    async fn life_and_version(pool: &PgPool, id: i32) -> (i32, i64) {
        sqlx::query_as("SELECT life_cur, save_version FROM players WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_newer_version_is_applied_with_its_skills(pool: PgPool) {
        let id = a_character(&pool, "a@example.com", "Rizael").await;

        let results = apply(&pool, &[a_save(id, 1)]).await.unwrap();

        assert_eq!(results.results, vec![SaveResult { id, outcome: SaveOutcome::Applied }]);
        assert_eq!(life_and_version(&pool, id).await, (37, 1));
        let skills: Vec<(i16, i16, i64)> = sqlx::query_as(
            "SELECT skill_type, value, current_ticks FROM player_skills WHERE player_id = $1",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(skills, vec![(0, 8, 1234)]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_equal_or_older_version_is_stale_and_changes_nothing(pool: PgPool) {
        let id = a_character(&pool, "a@example.com", "Rizael").await;
        apply(&pool, &[a_save(id, 2)]).await.unwrap();
        let mut older = a_save(id, 2);
        older.life.current = 1;

        let results = apply(&pool, &[older]).await.unwrap();

        assert_eq!(results.results, vec![SaveResult { id, outcome: SaveOutcome::Stale }]);
        assert_eq!(life_and_version(&pool, id).await, (37, 2));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_missing_or_deleted_character_is_gone(pool: PgPool) {
        let deleted = a_character(&pool, "a@example.com", "Rizael").await;
        sqlx::query("UPDATE players SET deleted_at = NOW() WHERE id = $1")
            .bind(deleted)
            .execute(&pool)
            .await
            .unwrap();

        let results = apply(&pool, &[a_save(deleted, 1), a_save(999_999, 1)]).await.unwrap();

        assert_eq!(
            results.results,
            vec![
                SaveResult { id: deleted, outcome: SaveOutcome::Gone },
                SaveResult { id: 999_999, outcome: SaveOutcome::Gone },
            ]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_failure_anywhere_in_the_batch_rolls_all_of_it_back(pool: PgPool) {
        let first = a_character(&pool, "a@example.com", "Rizael").await;
        let second = a_character(&pool, "b@example.com", "Anaia").await;
        let mut duplicate_skill = a_save(second, 1);
        duplicate_skill.skills.push(SkillRow { skill_type: 0, value: 9, current_ticks: 0 });

        assert!(apply(&pool, &[a_save(first, 1), duplicate_skill]).await.is_err());

        assert_eq!(life_and_version(&pool, first).await.1, 0);
    }
}
