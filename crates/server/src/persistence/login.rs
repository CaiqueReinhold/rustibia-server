//! Turning an auth token into a loaded player.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rustibia_contract::{CharacterRecord, StoredItemRecord};
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
use crate::actors::persistence::PersistenceActorHandle;
use crate::persistence::player::PlayerSnapshot;
use crate::persistence::site_client::{SiteClient, SiteError};

const DELIVERY_BEFORE_LOGIN: Duration = Duration::from_secs(5);

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
        save_version: record.save_version,
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

/// Login by calling the site over mutual TLS.
pub struct Login {
    site: Arc<SiteClient>,
    items: Arc<HashMap<ItemId, Arc<ItemConfig>>>,
    persistence: PersistenceActorHandle,
}

impl Login {
    pub fn new(
        site: Arc<SiteClient>,
        items: Arc<HashMap<ItemId, Arc<ItemConfig>>>,
        persistence: PersistenceActorHandle,
    ) -> Self {
        Self {
            site,
            items,
            persistence,
        }
    }

    /// Spends `auth_token` and returns the character it names, refusing with `Unavailable`
    /// while that character has a save the site has not acknowledged.
    /// Returns `Rejected` for every refusal without distinguishing them — the caller has
    /// no use for the difference and the site deliberately does not report it.
    pub async fn redeem(&self, auth_token: &str) -> Result<Box<PlayerSnapshot>, LoginError> {
        if let Some(character_id) = character_id_of(auth_token) {
            let delivered = tokio::time::timeout(
                DELIVERY_BEFORE_LOGIN,
                self.persistence.deliver_for(character_id),
            )
            .await
            .unwrap_or(false);
            if !delivered {
                return Err(LoginError::Unavailable(format!(
                    "the pending save for character {character_id} could not be delivered"
                )));
            }
        }

        let snapshot = match self.site.redeem(auth_token).await {
            Ok(record) => snapshot_from_record(record, &self.items)?,
            Err(SiteError::NotFound) => return Err(LoginError::Rejected),
            Err(SiteError::Unavailable(detail)) => return Err(LoginError::Unavailable(detail)),
        };

        if let Some(journaled) = self.persistence.pending_version(snapshot.id).await
            && journaled > snapshot.save_version
        {
            return Err(LoginError::Unavailable(format!(
                "character {} has save {journaled} journaled but the site returned {}",
                snapshot.id, snapshot.save_version
            )));
        }
        Ok(snapshot)
    }
}

pub fn character_id_of(auth_token: &str) -> Option<PlayerId> {
    let (id, _) = auth_token.split_once('.')?;
    id.parse().ok().map(PlayerId)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Facing;
    use crate::persistence::test_fixtures::no_items;
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
            save_version: 0,
        }
    }

    #[test]
    fn a_record_s_save_version_reaches_the_snapshot() {
        let mut record = a_record();
        record.save_version = 11;

        let snapshot = snapshot_from_record(record, &HashMap::new()).unwrap();

        assert_eq!(snapshot.save_version, 11);
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
}

/// `Login` against a mock site. These prove the status-code mapping and the journal gate,
/// and nothing else — a mock will happily return a body the real site would never produce,
/// which is exactly why `rustibia-contract` exists and why `internal_tls` on the site
/// side runs against the real router.
#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::entities::agent::Facing;
    use crate::persistence::test_fixtures::{a_character_record_json as a_record_json, no_items};
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Plain HTTP: TLS is the site's to prove, and mixing it in here would make every
    /// status-mapping test depend on a handshake.
    fn repo(server: &MockServer) -> Login {
        login_with(server, PersistenceActorHandle::nothing_pending())
    }

    fn login_with(server: &MockServer, persistence: PersistenceActorHandle) -> Login {
        Login::new(
            Arc::new(SiteClient::new(&server.uri(), reqwest::Client::new())),
            no_items(),
            persistence,
        )
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

        let repo = Login::new(
            Arc::new(SiteClient::new(
                &format!("http://127.0.0.1:{closed_port}"),
                reqwest::Client::new(),
            )),
            no_items(),
            PersistenceActorHandle::nothing_pending(),
        );

        assert!(matches!(
            repo.redeem("a-token").await,
            Err(LoginError::Unavailable(_))
        ));
    }

    /// Each delivery the courier was asked for: the character, and how many redemptions the
    /// site had already seen at that moment.
    type Deliveries = Arc<std::sync::Mutex<Vec<(u32, usize)>>>;

    fn a_courier(
        server: Arc<MockServer>,
        delivered: bool,
        journaled: Option<i64>,
    ) -> (PersistenceActorHandle, Deliveries) {
        use crate::actors::persistence::PersistenceCommand;

        let deliveries = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (persistence, mut rx) = PersistenceActorHandle::for_test();
        let log = Arc::clone(&deliveries);
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    PersistenceCommand::DeliverFor(id, reply) => {
                        let seen = server.received_requests().await.map_or(0, |r| r.len());
                        log.lock().unwrap().push((id.0, seen));
                        let _ = reply.send(delivered);
                    }
                    PersistenceCommand::PendingVersion(_, reply) => {
                        let _ = reply.send(journaled);
                    }
                    _ => {}
                }
            }
        });
        (persistence, deliveries)
    }

    async fn redemptions(server: &MockServer) -> usize {
        server.received_requests().await.map_or(0, |r| r.len())
    }

    #[tokio::test]
    async fn the_pending_save_is_delivered_before_the_token_is_redeemed() {
        let server = Arc::new(responding(200, a_record_json()).await);
        let (courier, deliveries) = a_courier(Arc::clone(&server), true, None);

        assert!(login_with(&server, courier).redeem("7.abc").await.is_ok());

        assert_eq!(*deliveries.lock().unwrap(), vec![(7, 0)]);
        assert_eq!(redemptions(&server).await, 1);
    }

    #[tokio::test]
    async fn an_undeliverable_save_refuses_the_login_without_spending_the_token() {
        let server = Arc::new(responding(200, a_record_json()).await);
        let (courier, _) = a_courier(Arc::clone(&server), false, Some(4));

        let result = login_with(&server, courier).redeem("7.abc").await;

        assert!(matches!(result, Err(LoginError::Unavailable(_))));
        assert_eq!(redemptions(&server).await, 0);
    }

    #[tokio::test]
    async fn a_journaled_version_newer_than_the_record_refuses_the_login() {
        let mut record = a_record_json();
        record["save_version"] = 3.into();
        let server = Arc::new(responding(200, record).await);
        let (courier, _) = a_courier(Arc::clone(&server), true, Some(5));

        let result = login_with(&server, courier).redeem("7.abc").await;

        assert!(matches!(result, Err(LoginError::Unavailable(_))));
    }

    #[tokio::test]
    async fn a_token_that_names_no_character_goes_straight_to_redemption() {
        let server = Arc::new(responding(200, a_record_json()).await);
        let (courier, deliveries) = a_courier(Arc::clone(&server), true, None);

        assert!(login_with(&server, courier).redeem("abc").await.is_ok());

        assert!(deliveries.lock().unwrap().is_empty());
        assert_eq!(redemptions(&server).await, 1);
    }

    #[test]
    fn a_character_id_is_read_from_before_the_first_dot() {
        assert_eq!(character_id_of("12.abc"), Some(PlayerId(12)));
        assert_eq!(character_id_of("12.abc.def"), Some(PlayerId(12)));
        assert_eq!(character_id_of("abc"), None);
        assert_eq!(character_id_of("x.abc"), None);
        assert_eq!(character_id_of(".abc"), None);
    }
}
