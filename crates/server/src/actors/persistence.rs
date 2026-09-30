use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use rustibia_contract::{CharacterSave, MAX_SAVE_BATCH, SaveBatch, SaveOutcome};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};
use tracing::{error, info, warn};

use crate::entities::player::PlayerId;
use crate::persistence::journal::Journal;
use crate::persistence::player::{PlayerSnapshot, to_character_save};
use crate::persistence::site_client::SiteClient;
use crate::telemetry;

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum PersistenceCommand {
    SavePlayer(Box<PlayerSnapshot>),
    MarkOnline(PlayerId),
    MarkOffline(PlayerId),
    ResetOnline,
    /// Replies `true` once nothing is journaled for the character.
    DeliverFor(PlayerId, oneshot::Sender<bool>),
    PendingVersion(PlayerId, oneshot::Sender<Option<i64>>),
    /// Replies once every save and online event has been delivered.
    Drain(oneshot::Sender<()>),
}

#[derive(Clone, Debug)]
pub struct PersistenceActorHandle {
    tx: mpsc::UnboundedSender<PersistenceCommand>,
}

impl PersistenceActorHandle {
    pub fn save_player(&self, player: Box<PlayerSnapshot>) {
        let _ = self.tx.send(PersistenceCommand::SavePlayer(player));
    }

    pub fn mark_online(&self, character_id: PlayerId) {
        let _ = self.tx.send(PersistenceCommand::MarkOnline(character_id));
    }

    pub fn mark_offline(&self, character_id: PlayerId) {
        let _ = self.tx.send(PersistenceCommand::MarkOffline(character_id));
    }

    pub fn reset_online(&self) {
        let _ = self.tx.send(PersistenceCommand::ResetOnline);
    }

    pub async fn deliver_for(&self, character_id: PlayerId) -> bool {
        let (reply, answer) = oneshot::channel();
        if self
            .tx
            .send(PersistenceCommand::DeliverFor(character_id, reply))
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    pub async fn pending_version(&self, character_id: PlayerId) -> Option<i64> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(PersistenceCommand::PendingVersion(character_id, reply))
            .ok()?;
        answer.await.ok().flatten()
    }

    pub async fn drain(&self) {
        let (reply, answer) = oneshot::channel();
        if self.tx.send(PersistenceCommand::Drain(reply)).is_ok() {
            let _ = answer.await;
        }
    }

    /// A handle with no actor behind it, plus the receiving end so a test can assert
    /// what was sent, or answer it.
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::UnboundedReceiver<PersistenceCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (PersistenceActorHandle { tx }, rx)
    }

    /// A handle whose courier has nothing journaled: every delivery succeeds at once, and
    /// everything else is dropped.
    #[cfg(test)]
    pub fn nothing_pending() -> Self {
        let (handle, mut rx) = Self::for_test();
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    PersistenceCommand::DeliverFor(_, reply) => {
                        let _ = reply.send(true);
                    }
                    PersistenceCommand::PendingVersion(_, reply) => {
                        let _ = reply.send(None);
                    }
                    _ => {}
                }
            }
        });
        handle
    }
}

#[derive(Debug)]
enum OnlineEvent {
    Online(i32),
    Offline(i32),
    Reset,
}

pub struct PersistenceActor {
    rx: mpsc::UnboundedReceiver<PersistenceCommand>,
    site: Arc<SiteClient>,
    journal: Journal,
    pending: BTreeMap<i32, CharacterSave>,
    online: VecDeque<OnlineEvent>,
    drain_waiters: Vec<oneshot::Sender<()>>,
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl PersistenceActor {
    /// `recovered` is what `journal` held at boot; it is delivered on the first `Drain`.
    pub fn start(
        site: Arc<SiteClient>,
        journal: Journal,
        recovered: Vec<CharacterSave>,
    ) -> PersistenceActorHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Self {
            rx,
            site,
            journal,
            pending: recovered.into_iter().map(|save| (save.id, save)).collect(),
            online: VecDeque::new(),
            drain_waiters: Vec::new(),
            retry_at: None,
            backoff: FIRST_RETRY,
        };
        tokio::spawn(actor.run());
        PersistenceActorHandle { tx }
    }

    async fn run(mut self) {
        info!("Persistence actor started");
        loop {
            let retry_at = self.retry_at;
            tokio::select! {
                command = self.rx.recv() => match command {
                    Some(command) => self.handle(command).await,
                    None => break,
                },
                () = async {
                    match retry_at {
                        Some(at) => sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => self.deliver().await,
            }
        }
        info!("Persistence actor stopped");
    }

    async fn handle(&mut self, command: PersistenceCommand) {
        match command {
            PersistenceCommand::SavePlayer(snapshot) => {
                let save = to_character_save(&snapshot);
                if let Err(e) = self.journal.write(&save) {
                    error!(
                        character_id = save.id,
                        "Could not journal a save; delivering it unjournaled: {e}"
                    );
                    telemetry::metrics().record_journal_failure();
                }
                self.pending.insert(save.id, save);
                self.nudge().await;
            }
            PersistenceCommand::MarkOnline(id) => {
                self.online.push_back(OnlineEvent::Online(id.0 as i32));
                self.nudge().await;
            }
            PersistenceCommand::MarkOffline(id) => {
                self.online.push_back(OnlineEvent::Offline(id.0 as i32));
                self.nudge().await;
            }
            PersistenceCommand::ResetOnline => {
                self.online.push_back(OnlineEvent::Reset);
                self.nudge().await;
            }
            PersistenceCommand::DeliverFor(id, reply) => {
                let id = id.0 as i32;
                let delivered = match self.pending.get(&id).cloned() {
                    Some(save) => self.send_saves(vec![save]).await && !self.pending.contains_key(&id),
                    None => true,
                };
                let _ = reply.send(delivered);
            }
            PersistenceCommand::PendingVersion(id, reply) => {
                let _ = reply.send(self.pending.get(&(id.0 as i32)).map(|save| save.save_version));
            }
            PersistenceCommand::Drain(reply) => {
                self.drain_waiters.push(reply);
                self.deliver().await;
            }
        }
    }

    async fn nudge(&mut self) {
        if self.retry_at.is_none() {
            self.deliver().await;
        }
    }

    async fn deliver(&mut self) {
        let delivered = self.send_online().await && self.send_all_saves().await;
        if delivered {
            self.retry_at = None;
            self.backoff = FIRST_RETRY;
            for waiter in self.drain_waiters.drain(..) {
                let _ = waiter.send(());
            }
        } else {
            self.retry_at = Some(Instant::now() + self.backoff);
            self.backoff = (self.backoff * 2).min(MAX_RETRY);
        }
        telemetry::metrics().record_saves_pending(self.pending.len() as u64);
    }

    async fn send_online(&mut self) -> bool {
        while let Some(event) = self.online.front() {
            let result = match *event {
                OnlineEvent::Online(id) => self.site.mark_online(id).await,
                OnlineEvent::Offline(id) => self.site.mark_offline(id).await,
                OnlineEvent::Reset => self.site.reset_online().await,
            };
            if let Err(e) = result {
                error!(?event, "Could not deliver an online event, retrying: {e}");
                return false;
            }
            self.online.pop_front();
        }
        true
    }

    async fn send_all_saves(&mut self) -> bool {
        let pending: Vec<CharacterSave> = self.pending.values().cloned().collect();
        for batch in pending.chunks(MAX_SAVE_BATCH) {
            if !self.send_saves(batch.to_vec()).await {
                return false;
            }
        }
        true
    }

    async fn send_saves(&mut self, characters: Vec<CharacterSave>) -> bool {
        let sent: HashMap<i32, i64> = characters
            .iter()
            .map(|save| (save.id, save.save_version))
            .collect();
        let results = match self.site.save(&SaveBatch { characters }).await {
            Ok(results) => results,
            Err(e) => {
                error!(characters = sent.len(), "Could not deliver saves, retrying: {e}");
                return false;
            }
        };
        for result in results.results {
            let Some(&version) = sent.get(&result.id) else {
                continue;
            };
            if result.outcome == SaveOutcome::Gone {
                warn!(
                    character_id = result.id,
                    "The site no longer has this character; dropping its save"
                );
            }
            if self
                .pending
                .get(&result.id)
                .is_some_and(|save| save.save_version <= version)
            {
                self.pending.remove(&result.id);
            }
            if let Err(e) = self.journal.remove_if_version(result.id, version) {
                error!(
                    character_id = result.id,
                    "Could not remove a delivered save from the journal: {e}"
                );
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::persistence::test_fixtures::a_test_snapshot;

    fn outcomes(pairs: &[(i32, &str)]) -> serde_json::Value {
        json!({
            "results": pairs
                .iter()
                .map(|(id, outcome)| json!({ "id": id, "outcome": outcome }))
                .collect::<Vec<_>>()
        })
    }

    async fn a_site_answering_saves(status: u16, body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/saves"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn a_courier(
        server: &MockServer,
        dir: &std::path::Path,
        recovered: Vec<CharacterSave>,
    ) -> PersistenceActorHandle {
        PersistenceActor::start(
            Arc::new(SiteClient::new(&server.uri(), reqwest::Client::new())),
            Journal::open(dir).unwrap(),
            recovered,
        )
    }

    fn a_snapshot(id: u32, save_version: i64) -> Box<PlayerSnapshot> {
        let mut snapshot = a_test_snapshot(id, 1);
        snapshot.save_version = save_version;
        Box::new(snapshot)
    }

    async fn saved_ids(server: &MockServer) -> Vec<Vec<i32>> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/internal/saves")
            .map(|request| {
                let batch: SaveBatch = serde_json::from_slice(&request.body).unwrap();
                batch.characters.iter().map(|save| save.id).collect()
            })
            .collect()
    }

    #[tokio::test]
    async fn an_acknowledged_save_leaves_nothing_journaled() {
        let server = a_site_answering_saves(200, outcomes(&[(1, "applied")])).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), Vec::new());

        courier.save_player(a_snapshot(1, 1));
        courier.drain().await;

        assert_eq!(saved_ids(&server).await, vec![vec![1]]);
        assert!(Journal::open(dir.path()).unwrap().pending().unwrap().is_empty());
        assert_eq!(courier.pending_version(PlayerId(1)).await, None);
    }

    #[tokio::test]
    async fn a_failed_delivery_keeps_the_save_journaled() {
        let server = a_site_answering_saves(500, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), Vec::new());

        courier.save_player(a_snapshot(1, 3));

        assert_eq!(courier.pending_version(PlayerId(1)).await, Some(3));
        let journaled = Journal::open(dir.path()).unwrap().pending().unwrap();
        assert_eq!(journaled.len(), 1);
        assert_eq!(journaled[0].save_version, 3);
    }

    #[tokio::test]
    async fn a_character_the_site_no_longer_has_is_dropped() {
        let server = a_site_answering_saves(200, outcomes(&[(1, "gone")])).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), Vec::new());

        courier.save_player(a_snapshot(1, 1));
        courier.drain().await;

        assert!(Journal::open(dir.path()).unwrap().pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delivering_for_one_character_sends_only_that_character() {
        let server = a_site_answering_saves(200, outcomes(&[(1, "applied")])).await;
        let dir = tempfile::tempdir().unwrap();
        let recovered = vec![
            to_character_save(&a_snapshot(1, 1)),
            to_character_save(&a_snapshot(2, 1)),
        ];
        let courier = a_courier(&server, dir.path(), recovered);

        assert!(courier.deliver_for(PlayerId(1)).await);

        assert_eq!(saved_ids(&server).await, vec![vec![1]]);
        assert_eq!(courier.pending_version(PlayerId(2)).await, Some(1));
    }

    #[tokio::test]
    async fn delivering_for_a_character_with_nothing_pending_sends_nothing() {
        let server = a_site_answering_saves(200, outcomes(&[])).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), Vec::new());

        assert!(courier.deliver_for(PlayerId(1)).await);

        assert!(saved_ids(&server).await.is_empty());
    }

    #[tokio::test]
    async fn a_failed_delivery_for_a_character_reports_false() {
        let server = a_site_answering_saves(500, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), vec![to_character_save(&a_snapshot(1, 1))]);

        assert!(!courier.deliver_for(PlayerId(1)).await);
    }

    #[tokio::test]
    async fn online_events_arrive_in_the_order_they_were_sent() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), Vec::new());

        courier.reset_online();
        courier.mark_online(PlayerId(1));
        courier.mark_offline(PlayerId(1));
        courier.drain().await;

        let calls: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| format!("{} {}", request.method, request.url.path()))
            .collect();
        assert_eq!(
            calls,
            vec![
                "POST /internal/online/reset",
                "POST /internal/online/1",
                "DELETE /internal/online/1",
            ]
        );
    }

    #[tokio::test]
    async fn a_save_that_cannot_be_journaled_is_still_delivered() {
        let server = a_site_answering_saves(200, outcomes(&[(1, "applied")])).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), Vec::new());
        std::fs::remove_dir_all(dir.path()).unwrap();

        courier.save_player(a_snapshot(1, 1));
        courier.drain().await;

        assert_eq!(saved_ids(&server).await, vec![vec![1]]);
    }
}
