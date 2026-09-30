//! The `online_players` rows behind the site's player count and Who Is Online list.

use sqlx::PgPool;

pub async fn mark_online(pool: &PgPool, character_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO online_players (character_id) \
         SELECT id FROM players WHERE id = $1 \
         ON CONFLICT (character_id) DO NOTHING",
    )
    .bind(character_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_offline(pool: &PgPool, character_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM online_players WHERE character_id = $1")
        .bind(character_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn reset(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM online_players").execute(pool).await?;
    Ok(())
}
