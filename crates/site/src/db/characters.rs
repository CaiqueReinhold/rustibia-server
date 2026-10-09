use sqlx::{PgPool, types::Json};
use time::OffsetDateTime;

use crate::{
    config::NewCharacterConfig,
    domain::{sex::Sex, vocation::Vocation},
    error::AppError,
};

#[derive(Debug, Clone)]
pub struct Character {
    pub id: i32,
    pub name: String,
    pub vocation: Vocation,
    pub sex: Sex,
    pub level: i16,
    pub online: bool,
    pub created_at: OffsetDateTime,
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct OnlineCharacter {
    pub name: String,
    pub vocation: Vocation,
    pub level: i16,
}

#[derive(Debug, Clone)]
pub struct HighscoreEntry {
    pub rank: i64,
    pub name: String,
    pub vocation: Vocation,
    pub level: i16,
}

const SKILL_TYPE_LEVEL: i16 = 0;

/// Creates a character and its starting skills in one transaction.
pub async fn create(
    pool: &PgPool,
    account_id: i32,
    name: &str,
    vocation: Vocation,
    sex: Sex,
    template: &NewCharacterConfig,
) -> Result<i32, AppError> {
    let kit = template.vocations.for_vocation(vocation);
    let outfit_id = kit.outfit_id(sex);

    let mut tx = pool.begin().await?;

    let character_id: i32 = sqlx::query_scalar(
        "INSERT INTO players \
         ( \
            account_id, \
            name, \
            vocation, \
            sex, \
            pos_x, \
            pos_y, \
            pos_z, \
            origin_x, \
            origin_y, \
            origin_z, \
            facing, \
            life_cur, \
            life_max, \
            mana_cur, \
            mana_max, \
            capacity, \
            speed, \
            outfit_id, \
            outfit_head, \
            outfit_body, \
            outfit_legs, \
            outfit_feet, \
            inventory \
        ) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$5,$6,$7,$8,$9,$9,$10,$10,$11,$12,$13,$14,$15,$16,$17,$18) \
         RETURNING id",
    )
    .bind(account_id)
    .bind(name)
    .bind(vocation.as_i16())
    .bind(sex.as_i16())
    .bind(template.pos_x)
    .bind(template.pos_y)
    .bind(template.pos_z)
    .bind(template.facing) //8
    .bind(template.life) //9
    .bind(template.mana) //10
    .bind(template.capacity) //11
    .bind(template.speed) //12
    .bind(outfit_id) //13
    .bind(template.outfit_head) //14
    .bind(template.outfit_body) //15
    .bind(template.outfit_legs) //16
    .bind(template.outfit_feet) //17
    .bind(Json(kit.stored_inventory())) //18
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Validation("That character name is already taken.".to_string())
        }
        _ => AppError::Database(e),
    })?;

    for skill in &template.starting_skills {
        sqlx::query(
            "INSERT INTO player_skills (player_id, skill_type, value, current_ticks) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(character_id)
        .bind(skill.skill_type)
        .bind(skill.value)
        .bind(skill.current_ticks)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(character_id)
}

/// Live characters for an account, with level and online status.
pub async fn list_for_account(pool: &PgPool, account_id: i32) -> Result<Vec<Character>, AppError> {
    let rows = sqlx::query_as::<_, (i32, String, i16, i16, Option<i16>, bool, OffsetDateTime)>(
        "SELECT p.id, p.name, p.vocation, p.sex, s.value, (o.character_id IS NOT NULL), p.created_at \
         FROM players p \
         LEFT JOIN player_skills s ON s.player_id = p.id AND s.skill_type = $2 \
         LEFT JOIN online_players o ON o.character_id = p.id \
         WHERE p.account_id = $1 AND p.deleted_at IS NULL \
         ORDER BY p.created_at",
    )
    .bind(account_id)
    .bind(SKILL_TYPE_LEVEL)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|(id, name, vocation, sex, level, online, created_at)| {
            Ok(Character {
                id,
                name,
                vocation: Vocation::from_i16(vocation)?,
                sex: Sex::from_i16(sex)?,
                level: level.unwrap_or(1),
                online,
                created_at,
                deleted: false,
            })
        })
        .collect()
}

/// `true` if the character exists, is not deleted, and belongs to this account.
pub async fn belongs_to_account(
    pool: &PgPool,
    character_id: i32,
    account_id: i32,
) -> Result<bool, AppError> {
    let found: Option<(i32,)> = sqlx::query_as(
        "SELECT id FROM players WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL",
    )
    .bind(character_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await?;

    Ok(found.is_some())
}

pub async fn is_online(pool: &PgPool, character_id: i32) -> Result<bool, AppError> {
    let found: Option<(i32,)> =
        sqlx::query_as("SELECT character_id FROM online_players WHERE character_id = $1")
            .bind(character_id)
            .fetch_optional(pool)
            .await?;

    Ok(found.is_some())
}

/// Soft-deletes a character. Returns `NotFound` if it is absent, already deleted,
/// owned by another account, **or currently online**.
pub async fn soft_delete(
    pool: &PgPool,
    character_id: i32,
    account_id: i32,
) -> Result<(), AppError> {
    let result = sqlx::query(
        "UPDATE players SET deleted_at = NOW() \
         WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM online_players o WHERE o.character_id = players.id)",
    )
    .bind(character_id)
    .bind(account_id)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Looks a character up by name, case-insensitively.
///
/// Deleted characters resolve too — the page shows a "this character has been
/// deleted" banner rather than pretending it never existed, which keeps old links
/// working and matches how tibia.com behaves.
pub async fn find_character_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<Character>, AppError> {
    let row = sqlx::query_as::<
        _,
        (
            i32,
            String,
            i16,
            i16,
            Option<i16>,
            bool,
            OffsetDateTime,
            bool,
        ),
    >(
        "SELECT p.id, p.name, p.vocation, p.sex, s.value, (o.character_id IS NOT NULL), \
                p.created_at, (p.deleted_at IS NOT NULL) \
         FROM players p \
         LEFT JOIN player_skills s ON s.player_id = p.id AND s.skill_type = $2 \
         LEFT JOIN online_players o ON o.character_id = p.id \
         WHERE lower(p.name) = lower($1)",
    )
    .bind(name)
    .bind(SKILL_TYPE_LEVEL)
    .fetch_optional(pool)
    .await?;

    row.map(
        |(id, name, vocation, sex, level, online, created_at, deleted)| {
            Ok(Character {
                id,
                name,
                vocation: Vocation::from_i16(vocation)?,
                sex: Sex::from_i16(sex)?,
                level: level.unwrap_or(1),
                online,
                created_at,
                deleted,
            })
        },
    )
    .transpose()
}

pub async fn who_is_online(pool: &PgPool) -> Result<Vec<OnlineCharacter>, AppError> {
    let rows = sqlx::query_as::<_, (String, i16, Option<i16>)>(
        "SELECT p.name, p.vocation, s.value \
         FROM online_players o \
         JOIN players p ON p.id = o.character_id \
         LEFT JOIN player_skills s ON s.player_id = p.id AND s.skill_type = $1 \
         WHERE p.deleted_at IS NULL \
         ORDER BY p.name",
    )
    .bind(SKILL_TYPE_LEVEL)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|(name, vocation, level)| {
            Ok(OnlineCharacter {
                name,
                vocation: Vocation::from_i16(vocation)?,
                level: level.unwrap_or(1),
            })
        })
        .collect()
}

/// Top `limit` characters by level, highest first. Excludes deleted characters.
pub async fn highscores(pool: &PgPool, limit: i64) -> Result<Vec<HighscoreEntry>, AppError> {
    let rows = sqlx::query_as::<_, (String, i16, i16)>(
        "SELECT p.name, p.vocation, s.value \
         FROM players p \
         JOIN player_skills s ON s.player_id = p.id AND s.skill_type = $1 \
         WHERE p.deleted_at IS NULL \
         ORDER BY s.value DESC, p.name ASC \
         LIMIT $2",
    )
    .bind(SKILL_TYPE_LEVEL)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .enumerate()
        .map(|(index, (name, vocation, level))| {
            Ok(HighscoreEntry {
                rank: index as i64 + 1,
                name,
                vocation: Vocation::from_i16(vocation)?,
                level,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::SiteConfig, db::accounts::create_account};
    use rustibia_contract::StoredItemRecord;
    use std::collections::HashMap;

    async fn a_character(pool: &PgPool, email: &str, name: &str, level: i16) -> i32 {
        let template = SiteConfig::load("config.yaml").unwrap().new_character;
        let account = create_account(pool, email, "hunter2hunter2").await.unwrap();
        let id = create(
            pool,
            account.id,
            name,
            Vocation::Knight,
            Sex::Male,
            &template,
        )
        .await
        .unwrap();

        sqlx::query("UPDATE player_skills SET value = $2 WHERE player_id = $1 AND skill_type = 0")
            .bind(id)
            .bind(level)
            .execute(pool)
            .await
            .unwrap();

        id
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn finds_a_character_case_insensitively(pool: PgPool) {
        a_character(&pool, "a@example.com", "Rizael", 8).await;

        let found = find_character_by_name(&pool, "rIzAeL")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            found.name, "Rizael",
            "the stored capitalisation is returned"
        );
        assert_eq!(found.level, 8);
        assert_eq!(found.vocation, Vocation::Knight);
        assert!(!found.deleted);
        assert!(!found.online);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_name_is_none(pool: PgPool) {
        assert!(
            find_character_by_name(&pool, "Nobody")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_deleted_character_still_resolves_and_is_flagged(pool: PgPool) {
        let id = a_character(&pool, "a@example.com", "Rizael", 5).await;
        sqlx::query("UPDATE players SET deleted_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let found = find_character_by_name(&pool, "Rizael")
            .await
            .unwrap()
            .unwrap();

        assert!(found.deleted, "old links must still resolve, with a banner");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn who_is_online_lists_only_connected_characters(pool: PgPool) {
        let online = a_character(&pool, "a@example.com", "Rizael", 3).await;
        a_character(&pool, "b@example.com", "Elyra", 4).await;

        sqlx::query("INSERT INTO online_players (character_id) VALUES ($1)")
            .bind(online)
            .execute(&pool)
            .await
            .unwrap();

        let listed = who_is_online(&pool).await.unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Rizael");
        assert_eq!(listed[0].level, 3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn who_is_online_excludes_deleted_characters(pool: PgPool) {
        let id = a_character(&pool, "a@example.com", "Rizael", 3).await;
        sqlx::query("INSERT INTO online_players (character_id) VALUES ($1)")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE players SET deleted_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        assert!(who_is_online(&pool).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn highscores_rank_by_level_descending(pool: PgPool) {
        a_character(&pool, "a@example.com", "Low", 2).await;
        a_character(&pool, "b@example.com", "High", 30).await;
        a_character(&pool, "c@example.com", "Mid", 10).await;

        let ranked = highscores(&pool, 100).await.unwrap();

        assert_eq!(ranked.len(), 3);
        assert_eq!(
            (ranked[0].rank, ranked[0].name.as_str(), ranked[0].level),
            (1, "High", 30)
        );
        assert_eq!(
            (ranked[1].rank, ranked[1].name.as_str(), ranked[1].level),
            (2, "Mid", 10)
        );
        assert_eq!(
            (ranked[2].rank, ranked[2].name.as_str(), ranked[2].level),
            (3, "Low", 2)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn highscores_break_ties_by_name_so_the_order_is_stable(pool: PgPool) {
        a_character(&pool, "a@example.com", "Zeta", 5).await;
        a_character(&pool, "b@example.com", "Alpha", 5).await;

        let ranked = highscores(&pool, 100).await.unwrap();

        assert_eq!(
            ranked[0].name, "Alpha",
            "equal levels must order by name, not at random"
        );
        assert_eq!(ranked[1].name, "Zeta");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn highscores_exclude_deleted_characters_and_honour_the_limit(pool: PgPool) {
        let deleted = a_character(&pool, "a@example.com", "Gone", 99).await;
        a_character(&pool, "b@example.com", "Kept", 1).await;
        sqlx::query("UPDATE players SET deleted_at = NOW() WHERE id = $1")
            .bind(deleted)
            .execute(&pool)
            .await
            .unwrap();

        let ranked = highscores(&pool, 100).await.unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].name, "Kept");

        a_character(&pool, "c@example.com", "Another", 50).await;
        assert_eq!(
            highscores(&pool, 1).await.unwrap().len(),
            1,
            "the limit must be honoured"
        );
    }

    fn template() -> NewCharacterConfig {
        SiteConfig::load("config.yaml").unwrap().new_character
    }

    async fn an_account(pool: &PgPool, email: &str) -> i32 {
        create_account(pool, email, "hunter2hunter2")
            .await
            .unwrap()
            .id
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn creates_a_character_with_its_starting_skills(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;

        let id = create(
            &pool,
            account_id,
            "Rizael",
            Vocation::Knight,
            Sex::Male,
            &template(),
        )
        .await
        .unwrap();

        let skills: Vec<(i16, i16)> = sqlx::query_as(
            "SELECT skill_type, value FROM player_skills WHERE player_id = $1 ORDER BY skill_type",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(
            skills.len(),
            template().starting_skills.len(),
            "every configured starting skill must be inserted"
        );
        assert_eq!(skills[0], (0, 1), "Level starts at 1");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn origin_matches_position_on_a_fresh_character(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let id = create(
            &pool,
            account_id,
            "Rizael",
            Vocation::Druid,
            Sex::Female,
            &template(),
        )
        .await
        .unwrap();

        let row: (i32, i32, i32, i32) =
            sqlx::query_as("SELECT pos_x, pos_y, origin_x, origin_y FROM players WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(
            (row.0, row.1),
            (row.2, row.3),
            "a new character starts at its origin"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn vocation_and_sex_select_the_outfit(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let t = template();

        let female = create(&pool, account_id, "Elyra", Vocation::Druid, Sex::Female, &t)
            .await
            .unwrap();
        let male = create(&pool, account_id, "Bahrun", Vocation::Knight, Sex::Male, &t)
            .await
            .unwrap();

        let outfit_of = |id: i32| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i16>("SELECT outfit_id FROM players WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };

        assert_eq!(outfit_of(female).await, t.vocations.druid.outfit_id_female);
        assert_eq!(outfit_of(male).await, t.vocations.knight.outfit_id_male);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_new_character_carries_its_vocations_kit(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let t = template();

        let id = create(
            &pool,
            account_id,
            "Rizael",
            Vocation::Paladin,
            Sex::Male,
            &t,
        )
        .await
        .unwrap();

        let Json(inventory): Json<HashMap<String, StoredItemRecord>> =
            sqlx::query_scalar("SELECT inventory FROM players WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(inventory, t.vocations.paladin.stored_inventory());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn rejects_a_duplicate_name_case_insensitively(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let t = template();
        create(&pool, account_id, "Rizael", Vocation::Knight, Sex::Male, &t)
            .await
            .unwrap();

        let err = create(
            &pool,
            account_id,
            "RIZAEL",
            Vocation::Druid,
            Sex::Female,
            &t,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, AppError::Validation(_)),
            "a taken name must be a validation error, not a 500; got {err:?}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_deleted_name_stays_reserved(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let t = template();
        let id = create(&pool, account_id, "Rizael", Vocation::Knight, Sex::Male, &t)
            .await
            .unwrap();

        soft_delete(&pool, id, account_id).await.unwrap();

        let err = create(
            &pool,
            account_id,
            "Rizael",
            Vocation::Druid,
            Sex::Female,
            &t,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, AppError::Validation(_)),
            "soft delete must keep the name reserved — the row still exists"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_online_character_cannot_be_soft_deleted(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let id = create(
            &pool,
            account_id,
            "Rizael",
            Vocation::Knight,
            Sex::Male,
            &template(),
        )
        .await
        .unwrap();

        sqlx::query("INSERT INTO online_players (character_id) VALUES ($1)")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        assert!(
            matches!(
                soft_delete(&pool, id, account_id).await,
                Err(AppError::NotFound)
            ),
            "the UPDATE itself must refuse an online character — a handler pre-check \
             alone races with a login"
        );
        assert_eq!(list_for_account(&pool, account_id).await.unwrap().len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn listing_excludes_deleted_characters(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let t = template();
        let kept = create(&pool, account_id, "Rizael", Vocation::Knight, Sex::Male, &t)
            .await
            .unwrap();
        let gone = create(&pool, account_id, "Elyra", Vocation::Druid, Sex::Female, &t)
            .await
            .unwrap();

        soft_delete(&pool, gone, account_id).await.unwrap();

        let listed = list_for_account(&pool, account_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, kept);
        assert_eq!(listed[0].level, 1);
        assert!(!listed[0].online);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn listing_reports_online_status(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let id = create(
            &pool,
            account_id,
            "Rizael",
            Vocation::Knight,
            Sex::Male,
            &template(),
        )
        .await
        .unwrap();

        sqlx::query("INSERT INTO online_players (character_id) VALUES ($1)")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        assert!(list_for_account(&pool, account_id).await.unwrap()[0].online);
        assert!(is_online(&pool, id).await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn another_account_cannot_see_or_delete_a_character(pool: PgPool) {
        let owner = an_account(&pool, "owner@example.com").await;
        let stranger = an_account(&pool, "stranger@example.com").await;
        let id = create(
            &pool,
            owner,
            "Rizael",
            Vocation::Knight,
            Sex::Male,
            &template(),
        )
        .await
        .unwrap();

        assert!(!belongs_to_account(&pool, id, stranger).await.unwrap());
        assert!(matches!(
            soft_delete(&pool, id, stranger).await,
            Err(AppError::NotFound)
        ));
        assert!(
            belongs_to_account(&pool, id, owner).await.unwrap(),
            "the failed deletion must not have affected the real owner's character"
        );
    }
}
