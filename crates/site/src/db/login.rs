//! Redeeming an auth token for the character record the game server loads.

use std::collections::HashMap;

use rustibia_contract::{CharacterRecord, Coords, Outfit, PoolValue, SkillRow, StoredItemRecord};
use sqlx::{PgPool, Row};

use crate::error::AppError;

/// Spends `token` and returns the character it names.
///
/// The caller does not choose the character — the token carries it, recorded at mint
/// time after the one ownership check this system performs. There is consequently no
/// "that character is not yours" refusal here: the request cannot express it.
///
/// `Ok(None)` covers every remaining refusal — unknown token, expired token,
/// already-redeemed token, soft-deleted character. The caller answers all of them with
/// the same 404, so distinguishing them here would only create something to
/// accidentally leak later.
pub async fn redeem(pool: &PgPool, token: &str) -> Result<Option<CharacterRecord>, AppError> {
    let mut tx = pool.begin().await?;

    let character_id: Option<i32> = sqlx::query_scalar(
        "DELETE FROM game_tokens WHERE token_hash = $1 AND valid_until > NOW() \
         RETURNING character_id",
    )
    .bind(crate::auth::token::hash_token(token))
    .fetch_optional(&mut *tx)
    .await?;

    let Some(character_id) = character_id else {
        // Nothing was deleted, so there is nothing to roll back; the explicit rollback
        // just returns the connection to the pool without waiting for the drop.
        tx.rollback().await?;
        return Ok(None);
    };

    let row = sqlx::query(
        "SELECT id, account_id, name, vocation, pos_x, pos_y, pos_z, origin_x, origin_y, origin_z, \
         facing, life_cur, life_max, mana_cur, mana_max, capacity, speed, \
         outfit_id, outfit_head, outfit_body, outfit_legs, outfit_feet, inventory, admin \
         FROM players WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(character_id)
    .fetch_optional(&mut *tx)
    .await?;

    // Reachable only for a character soft-deleted between mint and redemption. The
    // foreign key rules the other cases out: a token cannot exist for a character that
    // was never there, and the only hard delete is the cascade from `accounts`, which
    // takes this token with it in the same transaction.
    //
    // The rollback is the point regardless of how we got here — a refusal must leave the
    // token spendable, so a player whose load fails can still use the ticket they paid
    // for. Anything added below that can fail must stay inside this transaction.
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(None);
    };

    let skill_rows = sqlx::query(
        "SELECT skill_type, value, current_ticks FROM player_skills \
         WHERE player_id = $1",
    )
    .bind(character_id)
    .fetch_all(&mut *tx)
    .await?;

    let skills = skill_rows
        .iter()
        .map(|r| {
            Ok(SkillRow {
                skill_type: r.try_get("skill_type")?,
                value: r.try_get("value")?,
                current_ticks: r.try_get("current_ticks")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    let inventory: sqlx::types::Json<HashMap<String, StoredItemRecord>> =
        row.try_get("inventory")?;

    let record = CharacterRecord {
        id: row.try_get("id")?,
        account_id: row.try_get("account_id")?,
        admin: row.try_get("admin")?,
        name: row.try_get("name")?,
        vocation: row.try_get("vocation")?,
        position: Coords {
            x: row.try_get("pos_x")?,
            y: row.try_get("pos_y")?,
            z: row.try_get("pos_z")?,
        },
        origin: Coords {
            x: row.try_get("origin_x")?,
            y: row.try_get("origin_y")?,
            z: row.try_get("origin_z")?,
        },
        facing: row.try_get("facing")?,
        life: PoolValue {
            current: row.try_get("life_cur")?,
            maximum: row.try_get("life_max")?,
        },
        mana: PoolValue {
            current: row.try_get("mana_cur")?,
            maximum: row.try_get("mana_max")?,
        },
        capacity: row.try_get("capacity")?,
        speed: row.try_get("speed")?,
        outfit: Outfit {
            id: row.try_get("outfit_id")?,
            head: row.try_get("outfit_head")?,
            body: row.try_get("outfit_body")?,
            legs: row.try_get("outfit_legs")?,
            feet: row.try_get("outfit_feet")?,
        },
        skills,
        inventory: inventory.0,
    };

    tx.commit().await?;
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SiteConfig,
        db::{accounts::create_account, characters},
        domain::{sex::Sex, vocation::Vocation},
    };
    use time::{Duration, OffsetDateTime};

    async fn an_account(pool: &PgPool, email: &str) -> i32 {
        create_account(pool, email, "hunter2hunter2")
            .await
            .unwrap()
            .id
    }

    async fn a_character(pool: &PgPool, account_id: i32, name: &str) -> i32 {
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

    async fn a_token(pool: &PgPool, character_id: i32, ttl_seconds: i64) -> String {
        let token = format!("token-{}", uuid::Uuid::now_v7());
        sqlx::query(
            "INSERT INTO game_tokens (token_hash, character_id, valid_until) VALUES ($1, $2, $3)",
        )
        .bind(crate::auth::token::hash_token(&token))
        .bind(character_id)
        .bind(OffsetDateTime::now_utc() + Duration::seconds(ttl_seconds))
        .execute(pool)
        .await
        .unwrap();
        token
    }

    async fn token_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM game_tokens")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_valid_token_yields_the_character(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let token = a_token(&pool, character_id, 60).await;

        let record = redeem(&pool, &token)
            .await
            .unwrap()
            .expect("a live token must redeem");

        let template = SiteConfig::load("config.yaml").unwrap().new_character;
        assert_eq!(record.id, character_id);
        assert_eq!(record.account_id, account_id);
        assert_eq!(record.name, "Rizael");
        assert_eq!(record.position.x, template.pos_x);
        assert_eq!(record.origin.x, template.pos_x);
        assert_eq!(record.facing, template.facing);
        assert_eq!(record.life.current, template.life);
        assert_eq!(record.life.maximum, template.life);
        assert_eq!(record.outfit.id, template.outfit_id_male);
        assert_eq!(record.skills.len(), template.starting_skills.len());
        assert!(
            record.inventory.is_empty(),
            "a new character starts with nothing equipped"
        );
    }

    /// The property that replaces the old "someone else's character is refused" test.
    /// That request is no longer expressible, so what needs pinning is the positive
    /// form: the token decides which character loads, and nothing else can.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_token_loads_the_character_it_was_minted_for_and_no_other(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let first_id = a_character(&pool, account_id, "Rizael").await;
        let second_id = a_character(&pool, account_id, "Anaia").await;
        let first_token = a_token(&pool, first_id, 60).await;
        a_token(&pool, second_id, 60).await;

        let record = redeem(&pool, &first_token).await.unwrap().unwrap();

        assert_eq!(record.id, first_id);
        assert_eq!(record.name, "Rizael");
        assert_ne!(
            record.id, second_id,
            "the token, not the caller, chooses the character"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn redeeming_spends_the_token(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let token = a_token(&pool, character_id, 60).await;

        assert!(redeem(&pool, &token).await.unwrap().is_some());

        assert_eq!(token_count(&pool).await, 0, "the token must be deleted");
        assert!(
            redeem(&pool, &token).await.unwrap().is_none(),
            "a redeemed token must not work twice"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_token_is_refused(pool: PgPool) {
        assert!(redeem(&pool, "never-issued").await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_expired_token_is_refused_and_left_alone(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let token = a_token(&pool, character_id, -60).await;

        assert!(redeem(&pool, &token).await.unwrap().is_none());
        assert_eq!(
            token_count(&pool).await,
            1,
            "an expired token is not this function's to reap; deleting it here would \
             make the expiry path indistinguishable from the success path in the table"
        );
    }

    /// The failure that must not cost a token, and now the only route to it: the
    /// foreign key means a token cannot exist for a character that was never there, so
    /// a soft delete between mint and redeem is what reaches the rollback path.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_deleted_character_is_refused_without_spending_the_token(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let token = a_token(&pool, character_id, 60).await;
        characters::soft_delete(&pool, character_id, account_id)
            .await
            .unwrap();

        assert!(redeem(&pool, &token).await.unwrap().is_none());
        assert_eq!(
            token_count(&pool).await,
            1,
            "a failed load must roll back, leaving the token spendable"
        );
    }

    /// The inventory column is the one field whose JSON shape the two crates must agree
    /// on beyond column types, so it gets read back through the contract type.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_stored_inventory_survives_the_trip_through_jsonb(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;

        sqlx::query("UPDATE players SET inventory = $2 WHERE id = $1")
            .bind(character_id)
            .bind(sqlx::types::Json(serde_json::json!({
                "5": { "item_id": 2400, "amount": 1 },
                "3": { "item_id": 2148, "amount": 1,
                       "content": [{ "item_id": 2360, "amount": 10 }] }
            })))
            .execute(&pool)
            .await
            .unwrap();

        let token = a_token(&pool, character_id, 60).await;
        let record = redeem(&pool, &token).await.unwrap().unwrap();

        assert_eq!(record.inventory["5"].item_id, 2400);
        assert_eq!(record.inventory["5"].content, None);
        let bag = record.inventory["3"].content.as_ref().unwrap();
        assert_eq!(bag.len(), 1);
        assert_eq!(bag[0].item_id, 2360);
        assert_eq!(bag[0].amount, 10);
    }
}
