//! Bringing the journal to a state the server may start from.

use std::io;
use std::time::Duration;

use tracing::error;

use crate::persistence::journal::Journal;
use crate::persistence::site_client::{SiteClient, SiteError};

#[derive(Debug, PartialEq)]
pub enum Recovery {
    Clean,
    /// Characters whose logout is newer than any delivered world save.
    Leftovers(Vec<i32>),
}

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

/// Delivers a pending world save, then reports any logout it does not cover.
pub async fn recover(site: &SiteClient, journal: &Journal) -> io::Result<Recovery> {
    if let Some(save) = journal.world()? {
        until_delivered("the pending world save", async || {
            site.world_save(&save).await
        })
        .await;
        journal.remove_world()?;
        for player in journal.players()? {
            journal.remove_player_up_to(player.save.id, save.tick)?;
        }
    }
    let mut leftovers: Vec<i32> = journal
        .players()?
        .iter()
        .map(|player| player.save.id)
        .collect();
    leftovers.sort();
    Ok(match leftovers.is_empty() {
        true => Recovery::Clean,
        false => Recovery::Leftovers(leftovers),
    })
}

pub async fn until_delivered<T>(
    what: &str,
    mut attempt: impl AsyncFnMut() -> Result<T, SiteError>,
) -> T {
    let mut backoff = FIRST_RETRY;
    loop {
        match attempt().await {
            Ok(value) => return value,
            Err(e) => {
                error!("Could not deliver {what}, retrying in {backoff:?}: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_RETRY);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rustibia_contract::WorldSave;
    use serde_json::json;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::persistence::journal::JournaledPlayer;
    use crate::persistence::journal::tests::a_save;

    async fn a_site() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "skipped": [] })))
            .mount(&server)
            .await;
        server
    }

    fn site(server: &MockServer) -> SiteClient {
        SiteClient::new(&server.uri(), reqwest::Client::new())
    }

    #[tokio::test]
    async fn an_empty_journal_is_clean() {
        let server = a_site().await;
        let dir = tempfile::tempdir().unwrap();

        let recovery = recover(&site(&server), &Journal::open(dir.path()).unwrap())
            .await
            .unwrap();

        assert_eq!(recovery, Recovery::Clean);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pending_world_save_is_delivered_and_covers_older_logouts() {
        let server = a_site().await;
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        journal
            .write_world(&WorldSave {
                tick: 10,
                characters: Vec::new(),
                chunks: Vec::new(),
            })
            .unwrap();
        journal
            .write_player(&JournaledPlayer {
                tick: 9,
                save: a_save(7),
            })
            .unwrap();

        let recovery = recover(&site(&server), &journal).await.unwrap();

        assert_eq!(recovery, Recovery::Clean);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_eq!(journal.world().unwrap(), None);
    }

    #[tokio::test]
    async fn a_logout_newer_than_every_world_save_is_a_leftover() {
        let server = a_site().await;
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        journal
            .write_world(&WorldSave {
                tick: 10,
                characters: Vec::new(),
                chunks: Vec::new(),
            })
            .unwrap();
        journal
            .write_player(&JournaledPlayer {
                tick: 11,
                save: a_save(8),
            })
            .unwrap();
        journal
            .write_player(&JournaledPlayer {
                tick: 12,
                save: a_save(7),
            })
            .unwrap();

        let recovery = recover(&site(&server), &journal).await.unwrap();

        assert_eq!(recovery, Recovery::Leftovers(vec![7, 8]));
    }
}
