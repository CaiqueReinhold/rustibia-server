//! Turning an auth token into a loaded player.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rustibia_contract::{CharacterRecord, RedeemRequest, StoredItemRecord};
use sqlx::PgPool;
use thiserror::Error;
use tracing::warn;

use crate::entities::agent::{Facing, OutfitColors, OutfitId};
use crate::entities::player::PlayerId;
use crate::entities::vocation::Vocation;
use crate::entities::{
    agent::Pool,
    inventory::InventorySlot,
    items::{Item, ItemConfig, ItemId},
    position::Position,
    skills::{SkillType, SkillValue},
};
use crate::persistence::player::PlayerSnapshot;

#[derive(Debug, Error)]
pub enum LoginError {
    /// The token or the character was refused. The player can fix this by going back to
    /// the website for a new token, so it is not an operator's problem.
    #[error("token invalid or character not found")]
    Rejected,
    /// The login service could not be reached, or answered something unusable. Nothing
    /// the player does will help.
    #[error("login service unavailable: {0}")]
    Unavailable(String),
}

pub trait LoginRepository: Send + Sync {
    /// Spends `auth_token` and returns the character it names.
    /// Returns `Rejected` for every refusal without distinguishing them — the caller has
    /// no use for the difference and the site deliberately does not report it.
    fn redeem(
        &self,
        auth_token: &str,
    ) -> impl Future<Output = Result<Box<PlayerSnapshot>, LoginError>> + Send;
}

pub fn snapshot_from_record(
    record: CharacterRecord,
    items: &HashMap<ItemId, Arc<ItemConfig>>,
) -> Result<Box<PlayerSnapshot>, LoginError> {
    let id = u32::try_from(record.id)
        .map(PlayerId)
        .map_err(|_| malformed(format!("character id {} is negative", record.id)))?;

    let vocation = Vocation::from_i16(record.vocation)
        .ok_or_else(|| malformed(format!("unknown vocation {}", record.vocation)))?;

    let mut skills: HashMap<SkillType, SkillValue> = HashMap::new();
    for row in record.skills {
        let Some(skill_type) = SkillType::from_id(row.skill_type) else {
            warn!(
                character = %id,
                skill_type = row.skill_type,
                "ignoring a skill type this build does not know"
            );
            continue;
        };
        skills.insert(
            skill_type,
            SkillValue {
                value: row.value as u16,
                current_ticks: row.current_ticks as u64,
            },
        );
    }

    let mut inventory: HashMap<InventorySlot, Item> = HashMap::new();
    for (slot_str, stored) in record.inventory {
        let Ok(slot_id) = slot_str.parse::<u16>() else {
            warn!(character = %id, slot = %slot_str, "ignoring a non-numeric inventory slot");
            continue;
        };
        let Some(slot) = InventorySlot::from_id(slot_id) else {
            warn!(
                character = %id,
                slot = slot_id,
                "ignoring an unknown inventory slot"
            );
            continue;
        };
        if let Some(item) = restore_item(items, stored) {
            inventory.insert(slot, item);
        }
    }

    Ok(Box::new(PlayerSnapshot {
        id,
        account_id: record.account_id,
        admin: record.admin,
        name: record.name,
        vocation,
        position: coords(record.position, "position")?,
        origin: coords(record.origin, "origin")?,
        facing: Facing::from_id(record.facing)
            .ok_or_else(|| malformed(format!("unknown facing discriminant {}", record.facing)))?,
        life: pool(record.life, "life")?,
        mana: pool(record.mana, "mana")?,
        capacity: u32::try_from(record.capacity).map_err(|_| malformed("capacity out of range"))?,
        speed: u16::try_from(record.speed).map_err(|_| malformed("speed out of range"))?,
        outfit: (
            u16::try_from(record.outfit.id)
                .map(OutfitId)
                .map_err(|_| malformed("outfit id out of range"))?,
            OutfitColors {
                head: colour(record.outfit.head, "outfit head")?,
                body: colour(record.outfit.body, "outfit body")?,
                legs: colour(record.outfit.legs, "outfit legs")?,
                feet: colour(record.outfit.feet, "outfit feet")?,
            },
        ),
        skills,
        inventory,
    }))
}

fn coords(c: rustibia_contract::Coords, what: &str) -> Result<Position, LoginError> {
    Ok(Position {
        x: u16::try_from(c.x).map_err(|_| malformed(format!("{what} x {} out of range", c.x)))?,
        y: u16::try_from(c.y).map_err(|_| malformed(format!("{what} y {} out of range", c.y)))?,
        z: u8::try_from(c.z).map_err(|_| malformed(format!("{what} z {} out of range", c.z)))?,
    })
}

fn pool(p: rustibia_contract::PoolValue, what: &str) -> Result<Pool, LoginError> {
    Ok(Pool {
        current: u32::try_from(p.current)
            .map_err(|_| malformed(format!("{what} current {} is negative", p.current)))?,
        maximum: u32::try_from(p.maximum)
            .map_err(|_| malformed(format!("{what} maximum {} is negative", p.maximum)))?,
    })
}

fn colour(value: i16, what: &str) -> Result<u8, LoginError> {
    u8::try_from(value).map_err(|_| malformed(format!("{what} {value} out of range")))
}

fn malformed(detail: impl std::fmt::Display) -> LoginError {
    LoginError::Unavailable(format!("unusable character record: {detail}"))
}

/// The value stored in `game_tokens.token_hash`: SHA-256 of the token, hex-encoded.
///
/// Used only by `SqlLoginRepository`, which is the only part of this process that still
/// looks a token up in the database — the HTTP path sends the token to the site and the
/// site hashes it there.
///
/// This **must** match `hash_token` in `crates/site/src/auth/token.rs`. Duplicated rather
/// than shared because the only crate both processes link is `rustibia-contract`, whose
/// entire dependency list is `serde` and which holds no logic; adding a hash function
/// there would give both processes a transitive `sha2` for the sake of six lines. The
/// duplication is pinned instead: both sides assert the same known digest below, so a
/// change on either side fails that side's tests rather than silently rejecting logins.
fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// So `test_fixtures` can mint a token the way the site does. Deliberately not `pub`
/// beyond tests: nothing outside this module should be hashing tokens.
#[cfg(test)]
pub fn hash_token_for_tests(token: &str) -> String {
    hash_token(token)
}

/// Rebuilds an `Item` tree, dropping anything whose id this build has no configuration
/// for. Same tolerance the old load path had: an item removed from `assets/items/` should
/// cost the player that item, not their character.
fn restore_item(
    items: &HashMap<ItemId, Arc<ItemConfig>>,
    stored: StoredItemRecord,
) -> Option<Item> {
    let config = match items.get(&ItemId(stored.item_id)) {
        Some(c) => c.clone(),
        None => {
            warn!(
                item_id = stored.item_id,
                "skipping unknown item_id during inventory restore"
            );
            return None;
        }
    };

    let mut item = Item::new(config, stored.amount);
    if let Some(children) = stored.content {
        item.content = Some(Box::new(
            children
                .into_iter()
                .filter_map(|c| restore_item(items, c))
                .collect(),
        ));
    }
    Some(item)
}

/// Login straight against the database, bypassing the site.
///
/// Retained as the rollback path if the REST hop has to be backed out, and as the way
/// tests exercise a real login without standing up an HTTP server. It duplicates the
/// site's transaction on purpose: the two implementations of this trait have to make the
/// same promises about the token, or the seam is not a seam.
///
/// Unused by `main` on purpose — that is what "rollback path" means. Deleting it would
/// leave `LoginRepository` with one implementation and no evidence that the trait
/// abstracts anything.
pub struct SqlLoginRepository {
    pool: PgPool,
    items: Arc<HashMap<ItemId, Arc<ItemConfig>>>,
}

#[allow(dead_code)]
impl SqlLoginRepository {
    pub fn new(pool: PgPool, items: Arc<HashMap<ItemId, Arc<ItemConfig>>>) -> Self {
        Self { pool, items }
    }

    async fn redeem_inner(&self, auth_token: &str) -> Result<Box<PlayerSnapshot>, LoginError> {
        use sqlx::Row;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| LoginError::Unavailable(e.to_string()))?;

        let character_id: Option<i32> = sqlx::query_scalar(
            "DELETE FROM game_tokens WHERE token_hash = $1 AND valid_until > NOW() \
             RETURNING character_id",
        )
        .bind(hash_token(auth_token))
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| LoginError::Unavailable(e.to_string()))?;

        let Some(character_id) = character_id else {
            return Err(LoginError::Rejected);
        };

        let row = sqlx::query(
            "SELECT id, account_id, name, vocation, pos_x, pos_y, pos_z, origin_x, origin_y, origin_z, \
             facing, life_cur, life_max, mana_cur, mana_max, capacity, speed, \
             outfit_id, outfit_head, outfit_body, outfit_legs, outfit_feet, inventory \
             FROM players WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(character_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| LoginError::Unavailable(e.to_string()))?;

        // Dropping `tx` without committing rolls back, so the token survives a failed
        // load exactly as it does on the site's path.
        let Some(row) = row else {
            return Err(LoginError::Rejected);
        };

        let skill_rows = sqlx::query(
            "SELECT skill_type, value, current_ticks FROM player_skills \
             WHERE player_id = $1",
        )
        .bind(character_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| LoginError::Unavailable(e.to_string()))?;

        let record = (|| -> Result<CharacterRecord, sqlx::Error> {
            let inventory: sqlx::types::Json<HashMap<String, StoredItemRecord>> =
                row.try_get("inventory")?;

            Ok(CharacterRecord {
                vocation: row.try_get("vocation")?,
                id: row.try_get("id")?,
                account_id: row.try_get("account_id")?,
                admin: false,
                name: row.try_get("name")?,
                position: rustibia_contract::Coords {
                    x: row.try_get("pos_x")?,
                    y: row.try_get("pos_y")?,
                    z: row.try_get("pos_z")?,
                },
                origin: rustibia_contract::Coords {
                    x: row.try_get("origin_x")?,
                    y: row.try_get("origin_y")?,
                    z: row.try_get("origin_z")?,
                },
                facing: row.try_get("facing")?,
                life: rustibia_contract::PoolValue {
                    current: row.try_get("life_cur")?,
                    maximum: row.try_get("life_max")?,
                },
                mana: rustibia_contract::PoolValue {
                    current: row.try_get("mana_cur")?,
                    maximum: row.try_get("mana_max")?,
                },
                capacity: row.try_get("capacity")?,
                speed: row.try_get("speed")?,
                outfit: rustibia_contract::Outfit {
                    id: row.try_get("outfit_id")?,
                    head: row.try_get("outfit_head")?,
                    body: row.try_get("outfit_body")?,
                    legs: row.try_get("outfit_legs")?,
                    feet: row.try_get("outfit_feet")?,
                },
                skills: skill_rows
                    .iter()
                    .map(|r| {
                        Ok(rustibia_contract::SkillRow {
                            skill_type: r.try_get("skill_type")?,
                            value: r.try_get("value")?,
                            current_ticks: r.try_get("current_ticks")?,
                        })
                    })
                    .collect::<Result<Vec<_>, sqlx::Error>>()?,
                inventory: inventory.0,
            })
        })()
        .map_err(|e| LoginError::Unavailable(e.to_string()))?;

        let snapshot = snapshot_from_record(record, &self.items)?;

        tx.commit()
            .await
            .map_err(|e| LoginError::Unavailable(e.to_string()))?;

        Ok(snapshot)
    }
}

impl LoginRepository for SqlLoginRepository {
    fn redeem(
        &self,
        auth_token: &str,
    ) -> impl Future<Output = Result<Box<PlayerSnapshot>, LoginError>> + Send {
        self.redeem_inner(auth_token)
    }
}

/// How long to wait on the site before treating login as unavailable.
///
/// Short on purpose: the player is sitting on a connecting screen, and a login that is
/// going to fail should fail while they are still watching rather than after they have
/// given up and retried.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Login by calling the site over mutual TLS.
pub struct HttpLoginRepository {
    client: reqwest::Client,
    redeem_url: String,
    items: Arc<HashMap<ItemId, Arc<ItemConfig>>>,
}

impl HttpLoginRepository {
    /// `base_url` is the site's internal origin, e.g. `https://localhost:8443`.
    pub fn new(
        base_url: &str,
        client: reqwest::Client,
        items: Arc<HashMap<ItemId, Arc<ItemConfig>>>,
    ) -> Self {
        Self {
            client,
            redeem_url: format!(
                "{}/internal/game-tokens/redeem",
                base_url.trim_end_matches('/')
            ),
            items,
        }
    }

    /// Builds the mutual-TLS client, or fails.
    ///
    /// `add_root_certificate` with our CA and nothing else is deliberate — this client
    /// talks to exactly one host, and trusting the public root store would mean any CA on
    /// earth could impersonate the site. The identity is the other half: without it the
    /// site's verifier closes the connection.
    pub fn build_client(cert: &str, key: &str, ca: &str) -> Result<reqwest::Client, ClientError> {
        let mut identity = read(cert)?;
        identity.extend_from_slice(b"\n");
        identity.extend_from_slice(&read(key)?);

        let identity = reqwest::Identity::from_pem(&identity)
            .map_err(|e| ClientError::Identity(format!("{cert} + {key}"), e))?;
        let ca_cert = reqwest::Certificate::from_pem(&read(ca)?)
            .map_err(|e| ClientError::Certificate(ca.to_string(), e))?;

        reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca_cert)
            .identity(identity)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ClientError::Build)
    }

    async fn redeem_inner(&self, auth_token: &str) -> Result<Box<PlayerSnapshot>, LoginError> {
        let request = RedeemRequest {
            auth_token: auth_token.to_string(),
        };

        let response = self
            .client
            .post(&self.redeem_url)
            .json(&request)
            .send()
            .await
            .map_err(|e| LoginError::Unavailable(e.to_string()))?;

        match response.status() {
            reqwest::StatusCode::OK => {
                let record: CharacterRecord = response.json().await.map_err(|e| {
                    // A 200 whose body will not parse means the two sides disagree about
                    // the contract, which is a deployment mismatch and not the player's
                    // problem — hence Unavailable rather than Rejected.
                    LoginError::Unavailable(format!("unparseable character record: {e}"))
                })?;
                snapshot_from_record(record, &self.items)
            }
            reqwest::StatusCode::NOT_FOUND => Err(LoginError::Rejected),
            status => Err(LoginError::Unavailable(format!(
                "the site answered {status}"
            ))),
        }
    }
}

impl LoginRepository for HttpLoginRepository {
    fn redeem(
        &self,
        auth_token: &str,
    ) -> impl Future<Output = Result<Box<PlayerSnapshot>, LoginError>> + Send {
        self.redeem_inner(auth_token)
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("cannot read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("{0} is not a usable client identity: {1}")]
    Identity(String, reqwest::Error),
    #[error("{0} is not a usable certificate authority: {1}")]
    Certificate(String, reqwest::Error),
    #[error("building the internal HTTP client failed: {0}")]
    Build(reqwest::Error),
}

fn read(path: &str) -> Result<Vec<u8>, ClientError> {
    std::fs::read(path).map_err(|e| ClientError::Read(path.to_string(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Facing;
    use crate::persistence::player::PlayerRepository;
    use crate::persistence::test_fixtures::{
        a_test_snapshot, insert_account, insert_character, insert_token, insert_token_valid_for,
        no_items, token_count,
    };
    use rustibia_contract::{Coords, Outfit, PoolValue, SkillRow};

    fn a_record() -> CharacterRecord {
        CharacterRecord {
            id: 7,
            account_id: 3,
            admin: false,
            name: "Rizael".to_string(),
            vocation: 0,
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
            skills: vec![SkillRow {
                skill_type: 1,
                value: 220,
                current_ticks: 0,
            }],
            inventory: HashMap::new(),
        }
    }

    /// The same constant `crates/site/src/auth/token.rs` asserts. These two tests are the
    /// only thing tying the two hash implementations together — nothing here fails to
    /// compile if they diverge, it just stops accepting every token.
    #[test]
    fn hash_token_matches_the_sites_digest() {
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "this must equal site::auth::token::hash_token for the same input"
        );
    }

    #[test]
    fn a_record_becomes_a_snapshot() {
        let snapshot = snapshot_from_record(a_record(), &no_items()).unwrap();

        assert_eq!(snapshot.id, PlayerId(7));
        assert_eq!(snapshot.account_id, 3);
        assert_eq!(snapshot.name, "Rizael");
        assert_eq!(
            snapshot.position,
            Position {
                x: 1028,
                y: 1029,
                z: 7
            }
        );
        assert_eq!(
            snapshot.origin,
            Position {
                x: 1028,
                y: 1028,
                z: 7
            }
        );
        assert_eq!(snapshot.facing, Facing::South);
        assert_eq!(snapshot.life.current, 140);
        assert_eq!(snapshot.life.maximum, 150);
        assert_eq!(snapshot.capacity, 400);
        // Speed and capacity are plain columns sitting next to the outfit fields,
        // so a mis-shifted mapping reads a neighbour and still type-checks. Both
        // are pinned here against fixture values that differ from every neighbour.
        assert_eq!(snapshot.speed, 100);
        assert_eq!(
            snapshot.outfit,
            (OutfitId(128), OutfitColors::new(78, 69, 58, 76))
        );
    }

    #[test]
    fn an_unknown_facing_is_unavailable_not_rejected() {
        let mut record = a_record();
        record.facing = 9;

        let err = snapshot_from_record(record, &no_items()).unwrap_err();

        assert!(
            matches!(err, LoginError::Unavailable(_)),
            "the token is already spent by this point, so telling the player their token \
             was invalid would be wrong; got {err:?}"
        );
    }

    #[test]
    fn out_of_range_coordinates_are_refused() {
        let mut record = a_record();
        record.position.x = 70_000;

        assert!(matches!(
            snapshot_from_record(record, &no_items()).unwrap_err(),
            LoginError::Unavailable(_)
        ));
    }

    #[test]
    fn a_negative_pool_value_is_refused() {
        let mut record = a_record();
        record.life.current = -1;

        assert!(matches!(
            snapshot_from_record(record, &no_items()).unwrap_err(),
            LoginError::Unavailable(_)
        ));
    }

    /// The opposite policy to the checks above, and deliberately so: an unimplemented
    /// skill costs the player that skill, not their ability to log in.
    #[test]
    fn an_unknown_skill_type_is_dropped_rather_than_fatal() {
        let mut record = a_record();
        record.skills.push(SkillRow {
            skill_type: 99,
            value: 5,
            current_ticks: 0,
        });

        let snapshot = snapshot_from_record(record, &no_items()).unwrap();

        assert_eq!(snapshot.skills.len(), 1, "only the known skill survives");
    }

    #[test]
    fn an_item_with_no_configuration_is_dropped_rather_than_fatal() {
        let mut record = a_record();
        record.inventory.insert(
            "5".to_string(),
            StoredItemRecord {
                item_id: 9999,
                amount: 1,
                content: None,
            },
        );

        let snapshot = snapshot_from_record(record, &no_items()).unwrap();

        assert!(
            snapshot.inventory.is_empty(),
            "an item this build has no config for is skipped, not fatal"
        );
    }

    #[test]
    fn an_unknown_inventory_slot_is_dropped() {
        let mut record = a_record();
        record.inventory.insert(
            "not-a-number".to_string(),
            StoredItemRecord {
                item_id: 2360,
                amount: 1,
                content: None,
            },
        );

        assert!(
            snapshot_from_record(record, &no_items())
                .unwrap()
                .inventory
                .is_empty()
        );
    }

    // ---- SqlLoginRepository -------------------------------------------------------

    #[sqlx::test(migrations = "../site/migrations")]
    async fn sql_redeem_loads_the_character(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let character_id = insert_character(&pool, account_id).await;
        let token = insert_token(&pool, character_id).await;

        let repo = SqlLoginRepository::new(pool, no_items());
        let snapshot = repo.redeem(&token).await.unwrap();

        assert_eq!(snapshot.id, PlayerId(character_id as u32));
        assert_eq!(snapshot.account_id, account_id);
        assert_eq!(
            snapshot.position,
            Position {
                x: 1028,
                y: 1028,
                z: 7
            }
        );
        assert_eq!(snapshot.facing, Facing::South);
        assert_eq!(snapshot.life.maximum, 150);
    }

    /// The seam's half of "the token decides". The site proves this against its own
    /// query; the rollback path has to make the same promise or it is not a rollback
    /// path.
    #[sqlx::test(migrations = "../site/migrations")]
    async fn sql_redeem_loads_the_token_s_character_and_no_other(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let first_id = insert_character(&pool, account_id).await;
        let second_id = insert_character(&pool, account_id).await;
        let second_token = insert_token(&pool, second_id).await;
        insert_token(&pool, first_id).await;

        let repo = SqlLoginRepository::new(pool, no_items());
        let snapshot = repo.redeem(&second_token).await.unwrap();

        assert_eq!(snapshot.id, PlayerId(second_id as u32));
        assert_ne!(snapshot.id, PlayerId(first_id as u32));
    }

    #[sqlx::test(migrations = "../site/migrations")]
    async fn sql_redeem_spends_the_token(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let character_id = insert_character(&pool, account_id).await;
        let token = insert_token(&pool, character_id).await;

        let repo = SqlLoginRepository::new(pool.clone(), no_items());
        repo.redeem(&token).await.unwrap();

        assert_eq!(token_count(&pool).await, 0);
        assert!(
            matches!(repo.redeem(&token).await, Err(LoginError::Rejected)),
            "a redeemed token must not work twice"
        );
    }

    #[sqlx::test(migrations = "../site/migrations")]
    async fn sql_redeem_rejects_an_unknown_token(pool: PgPool) {
        let repo = SqlLoginRepository::new(pool, no_items());

        assert!(matches!(
            repo.redeem("never-issued").await,
            Err(LoginError::Rejected)
        ));
    }

    #[sqlx::test(migrations = "../site/migrations")]
    async fn sql_redeem_rejects_an_expired_token(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let character_id = insert_character(&pool, account_id).await;
        let token = insert_token_valid_for(&pool, character_id, "-1 hour").await;

        let repo = SqlLoginRepository::new(pool, no_items());

        assert!(matches!(
            repo.redeem(&token).await,
            Err(LoginError::Rejected)
        ));
    }

    /// The property that makes single use safe. Both implementations of the trait must
    /// have it, which is why the SQL one runs the same transaction as the site.
    ///
    /// A soft delete is the only way in now: the foreign key means a token cannot exist
    /// for a character that was never there, and a hard delete would cascade the token
    /// away with it.
    #[sqlx::test(migrations = "../site/migrations")]
    async fn sql_redeem_leaves_the_token_unspent_when_the_load_fails(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let character_id = insert_character(&pool, account_id).await;
        let token = insert_token(&pool, character_id).await;
        sqlx::query("UPDATE players SET deleted_at = NOW() WHERE id = $1")
            .bind(character_id)
            .execute(&pool)
            .await
            .unwrap();

        let repo = SqlLoginRepository::new(pool.clone(), no_items());

        assert!(matches!(
            repo.redeem(&token).await,
            Err(LoginError::Rejected)
        ));
        assert_eq!(
            token_count(&pool).await,
            1,
            "a character that cannot be loaded must not cost the player their token"
        );
    }

    #[sqlx::test(migrations = "../site/migrations")]
    async fn sql_redeem_rejects_a_soft_deleted_character(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let character_id = insert_character(&pool, account_id).await;
        sqlx::query("UPDATE players SET deleted_at = NOW() WHERE id = $1")
            .bind(character_id)
            .execute(&pool)
            .await
            .unwrap();
        let token = insert_token(&pool, character_id).await;

        let repo = SqlLoginRepository::new(pool, no_items());

        assert!(
            matches!(repo.redeem(&token).await, Err(LoginError::Rejected)),
            "a deleted character must not be able to log in"
        );
    }

    /// The round trip that matters: what `save` writes, `redeem` must read back.
    #[sqlx::test(migrations = "../site/migrations")]
    async fn what_save_writes_redeem_reads_back(pool: PgPool) {
        let account_id = insert_account(&pool).await;
        let character_id = insert_character(&pool, account_id).await;

        let mut snapshot = a_test_snapshot(character_id as u32, account_id);
        snapshot.position = Position {
            x: 200,
            y: 300,
            z: 5,
        };
        snapshot.life.current = 60;
        snapshot.facing = Facing::West;

        PlayerRepository::new(pool.clone())
            .save(&snapshot)
            .await
            .unwrap();

        let token = insert_token(&pool, character_id).await;
        let loaded = SqlLoginRepository::new(pool, no_items())
            .redeem(&token)
            .await
            .unwrap();

        assert_eq!(
            loaded.position,
            Position {
                x: 200,
                y: 300,
                z: 5
            }
        );
        assert_eq!(loaded.life.current, 60);
        assert_eq!(loaded.facing, Facing::West);
        assert_eq!(loaded.skills.len(), snapshot.skills.len());
    }
}

/// `HttpLoginRepository` against a mock site. These prove the status-code mapping and
/// nothing else — a mock will happily return a body the real site would never produce,
/// which is exactly why `rustibia-contract` exists and why `internal_tls` on the site
/// side runs against the real router.
#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::entities::agent::Facing;
    use crate::persistence::test_fixtures::no_items;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn a_record_json() -> serde_json::Value {
        serde_json::json!({
            "id": 7,
            "account_id": 3,
            "admin": false,
            "name": "Rizael",
            "vocation": 0,
            "position": { "x": 1028, "y": 1029, "z": 7 },
            "origin": { "x": 1028, "y": 1028, "z": 7 },
            "facing": 2,
            "life": { "current": 140, "maximum": 150 },
            "mana": { "current": 0, "maximum": 0 },
            "capacity": 400,
            "speed": 120,
            "outfit": { "id": 128, "head": 78, "body": 69, "legs": 58, "feet": 76 },
            "skills": [{ "skill_type": 1, "value": 220, "current_ticks": 0 }],
            "inventory": {}
        })
    }

    /// Plain HTTP: TLS is the site's to prove, and mixing it in here would make every
    /// status-mapping test depend on a handshake.
    fn repo(server: &MockServer) -> HttpLoginRepository {
        HttpLoginRepository::new(&server.uri(), reqwest::Client::new(), no_items())
    }

    async fn responding(status: u16, body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/game-tokens/redeem"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_200_becomes_a_snapshot() {
        let server = responding(200, a_record_json()).await;

        let snapshot = repo(&server).redeem("a-token").await.unwrap();

        assert_eq!(snapshot.id, PlayerId(7));
        assert_eq!(snapshot.name, "Rizael");
        assert_eq!(snapshot.facing, Facing::South);
        // The scalar shape of these two is the half of the contract this mock
        // owns: the site sending a pool or a string here fails to deserialize,
        // and nothing else in this module would notice.
        assert_eq!(snapshot.capacity, 400);
        assert_eq!(snapshot.speed, 120);
    }

    /// The request shape is half the contract. If the field name drifted, the site would
    /// answer 422 and every test above would still pass on its own mock.
    #[tokio::test]
    async fn the_request_carries_the_token_and_nothing_else() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/game-tokens/redeem"))
            .and(body_json(serde_json::json!({ "auth_token": "a-token" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(a_record_json()))
            .mount(&server)
            .await;

        assert!(repo(&server).redeem("a-token").await.is_ok());
    }

    #[tokio::test]
    async fn a_404_is_rejected() {
        let server = responding(404, serde_json::json!({ "error": "not found" })).await;

        assert!(matches!(
            repo(&server).redeem("a-token").await,
            Err(LoginError::Rejected)
        ));
    }

    #[tokio::test]
    async fn a_500_is_unavailable_and_never_rejected() {
        let server = responding(500, serde_json::json!({ "error": "boom" })).await;

        let err = repo(&server).redeem("a-token").await.unwrap_err();

        assert!(
            matches!(err, LoginError::Unavailable(_)),
            "a site failure must not be reported as the player's token being bad, got {err:?}"
        );
    }

    /// 401 is what the TLS layer would produce for an unauthenticated client. It is the
    /// game server's own misconfiguration, so it must not look like a player error.
    #[tokio::test]
    async fn a_401_is_unavailable() {
        let server = responding(401, serde_json::json!({ "error": "no certificate" })).await;

        assert!(matches!(
            repo(&server).redeem("a-token").await,
            Err(LoginError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_200_with_a_body_missing_a_field_is_unavailable() {
        let mut body = a_record_json();
        body.as_object_mut().unwrap().remove("facing");
        let server = responding(200, body).await;

        let err = repo(&server).redeem("a-token").await.unwrap_err();

        assert!(
            matches!(err, LoginError::Unavailable(_)),
            "a contract mismatch is a deployment problem, not a bad token; got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_200_that_is_not_json_is_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        assert!(matches!(
            repo(&server).redeem("a-token").await,
            Err(LoginError::Unavailable(_))
        ));
    }

    /// The failure mode the whole "fail closed" decision is about: the site is down.
    ///
    /// The address comes from a listener that is bound and then closed, rather than from
    /// a dropped `MockServer` — a dropped mock keeps answering 404 for a moment, which
    /// this test would have read as `Rejected` and passed on the wrong reason.
    #[tokio::test]
    async fn a_refused_connection_is_unavailable() {
        let closed_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };

        let repo = HttpLoginRepository::new(
            &format!("http://127.0.0.1:{closed_port}"),
            reqwest::Client::new(),
            no_items(),
        );

        assert!(matches!(
            repo.redeem("a-token").await,
            Err(LoginError::Unavailable(_))
        ));
    }

    #[test]
    fn the_url_is_built_without_a_double_slash() {
        let repo = HttpLoginRepository::new(
            "https://localhost:8443/",
            reqwest::Client::new(),
            no_items(),
        );

        assert_eq!(
            repo.redeem_url,
            "https://localhost:8443/internal/game-tokens/redeem"
        );
    }

    // ---- build_client ---------------------------------------------------------------

    struct Certs {
        dir: std::path::PathBuf,
    }

    impl Certs {
        fn generate(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rustibia-server-tls-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            rustibia_certgen::generate_bundle(&dir).unwrap();
            Self { dir }
        }

        fn path(&self, name: &str) -> String {
            self.dir.join(name).display().to_string()
        }
    }

    impl Drop for Certs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn build_client_accepts_a_generated_bundle() {
        let certs = Certs::generate("ok");

        assert!(
            HttpLoginRepository::build_client(
                &certs.path("server.crt"),
                &certs.path("server.key"),
                &certs.path("ca.crt"),
            )
            .is_ok()
        );
    }

    #[test]
    fn build_client_fails_on_a_missing_identity() {
        let certs = Certs::generate("missing-identity");

        let err = HttpLoginRepository::build_client(
            &certs.path("nope.crt"),
            &certs.path("server.key"),
            &certs.path("ca.crt"),
        )
        .expect_err("a missing client certificate must stop the process at boot");

        assert!(matches!(err, ClientError::Read(_, _)), "got {err:?}");
    }

    #[test]
    fn build_client_fails_on_a_missing_ca() {
        let certs = Certs::generate("missing-ca");

        assert!(
            HttpLoginRepository::build_client(
                &certs.path("server.crt"),
                &certs.path("server.key"),
                &certs.path("nope.crt"),
            )
            .is_err(),
            "without the CA this client would have to trust anything claiming to be the site"
        );
    }

    #[test]
    fn build_client_fails_on_a_key_that_is_not_pem() {
        let certs = Certs::generate("garbage-key");
        let garbage = certs.path("garbage.key");
        std::fs::write(&garbage, b"not a key").unwrap();

        assert!(
            HttpLoginRepository::build_client(
                &certs.path("server.crt"),
                &garbage,
                &certs.path("ca.crt"),
            )
            .is_err()
        );
    }
}
