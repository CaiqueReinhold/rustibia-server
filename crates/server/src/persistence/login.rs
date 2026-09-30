//! Turning an auth token into a loaded player.

use std::collections::HashMap;
use std::sync::Arc;

use rustibia_contract::{CharacterRecord, CharacterSave, StoredItemRecord};
use thiserror::Error;
use tracing::warn;

use crate::actors::persistence::PersistenceActorHandle;
use crate::entities::agent::{Facing, OutfitColors, OutfitId};
use crate::entities::player::PlayerId;
use crate::entities::vocation::Vocation;
use crate::entities::{
    agent::Pool,
    inventory::InventorySlot,
    items::{FluidType, Item, ItemConfig, ItemFlag, ItemId},
    position::Position,
    skills::{SkillType, SkillValue},
};
use crate::persistence::player::PlayerSnapshot;
use crate::persistence::site_client::{SiteClient, SiteError};

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
pub(crate) fn restore_item(
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

    let mut item = match config.has_flag(ItemFlag::LiquidPool) {
        true => match FluidType::from_u8(stored.amount) {
            Some(fluid) => Item::new_fluid(config, fluid),
            None => {
                warn!(
                    item_id = stored.item_id,
                    fluid = stored.amount,
                    "skipping an unknown fluid"
                );
                return None;
            }
        },
        false => Item::new(config, stored.amount),
    };
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

    /// Spends `auth_token` and returns the character it names, in its journaled state when
    /// the character logged out since the last world save.
    /// Returns `Rejected` for every refusal without distinguishing them — the caller has
    /// no use for the difference and the site deliberately does not report it.
    pub async fn redeem(&self, auth_token: &str) -> Result<Box<PlayerSnapshot>, LoginError> {
        let mut record = match self.site.redeem(auth_token).await {
            Ok(record) => record,
            Err(SiteError::NotFound) => return Err(LoginError::Rejected),
            Err(SiteError::Unavailable(detail)) => return Err(LoginError::Unavailable(detail)),
        };
        if let Some(save) = self.persistence.journaled_state(record.id).await {
            overlay(&mut record, save);
        }
        snapshot_from_record(record, &self.items)
    }
}

fn overlay(record: &mut CharacterRecord, save: CharacterSave) {
    record.position = save.position;
    record.origin = save.origin;
    record.facing = save.facing;
    record.life = save.life;
    record.mana = save.mana;
    record.capacity = save.capacity;
    record.speed = save.speed;
    record.outfit = save.outfit;
    record.skills = save.skills;
    record.inventory = save.inventory;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Facing;
    use crate::persistence::test_fixtures::no_items;
    use rustibia_contract::{Coords, Outfit, PoolValue, SkillRow};

    #[test]
    fn a_liquid_pool_round_trips_its_fluid_through_the_amount() {
        use crate::entities::items::{FluidType, ItemFlag};
        use crate::persistence::player::to_character_save;
        use crate::persistence::test_fixtures::a_test_snapshot;

        let pool_config = Arc::new(ItemConfig::new(
            ItemId(2886),
            "pool".to_string(),
            None,
            None,
            [ItemFlag::LiquidPool],
            Vec::new(),
        ));
        let mut snapshot = a_test_snapshot(7, 3);
        snapshot.inventory = HashMap::from([(
            InventorySlot::Backpack,
            Item::new_fluid(Arc::clone(&pool_config), FluidType::Slime),
        )]);
        let stored = to_character_save(&snapshot).inventory.remove("3").unwrap();

        let restored = restore_item(&HashMap::from([(ItemId(2886), pool_config)]), stored).unwrap();

        assert_eq!(
            (restored.fluid, restored.amount),
            (Some(FluidType::Slime), 1)
        );
    }

    #[test]
    fn a_stack_round_trips_its_count_through_the_amount() {
        let coin = Arc::new(ItemConfig::new(
            ItemId(3031),
            "coin".to_string(),
            None,
            None,
            Vec::<crate::entities::items::ItemFlag>::new(),
            Vec::new(),
        ));
        let stored = StoredItemRecord {
            item_id: 3031,
            amount: 42,
            content: None,
        };

        let restored = restore_item(&HashMap::from([(ItemId(3031), coin)]), stored).unwrap();

        assert_eq!((restored.fluid, restored.amount), (None, 42));
    }

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

    fn a_courier_holding(save: Option<rustibia_contract::CharacterSave>) -> PersistenceActorHandle {
        use crate::actors::persistence::PersistenceCommand;

        let (persistence, mut rx) = PersistenceActorHandle::for_test();
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                if let PersistenceCommand::JournaledState(_, reply) = command {
                    let _ = reply.send(save.clone());
                }
            }
        });
        persistence
    }

    #[tokio::test]
    async fn a_journaled_state_replaces_the_records_but_not_its_name() {
        let server = responding(200, a_record_json()).await;
        let mut journaled = crate::persistence::player::to_character_save(
            &crate::persistence::test_fixtures::a_test_snapshot(7, 3),
        );
        journaled.position = rustibia_contract::Coords {
            x: 500,
            y: 600,
            z: 6,
        };

        let snapshot = login_with(&server, a_courier_holding(Some(journaled)))
            .redeem("a-token")
            .await
            .unwrap();

        assert_eq!(
            (
                snapshot.position.x,
                snapshot.position.y,
                snapshot.position.z
            ),
            (500, 600, 6)
        );
        assert_eq!(snapshot.name, "Rizael");
    }

    #[tokio::test]
    async fn without_a_journaled_state_the_record_is_used() {
        let server = responding(200, a_record_json()).await;

        let snapshot = login_with(&server, a_courier_holding(None))
            .redeem("a-token")
            .await
            .unwrap();

        assert_eq!((snapshot.position.x, snapshot.position.y), (1028, 1029));
    }
}
