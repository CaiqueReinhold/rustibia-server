use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustibia_contract::{CharacterSave, WorldSave};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until};
use tracing::{error, info, warn};

use crate::entities::map::GameMap;
use crate::entities::player::PlayerId;
use crate::game::Tick;
use crate::persistence::journal::{Journal, JournaledPlayer};
use crate::persistence::player::{PlayerSnapshot, to_character_save};
use crate::persistence::site_client::SiteClient;
use crate::persistence::world_save::chunk_rows;
use crate::telemetry;

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum PersistenceCommand {
    /// A player's final state, and the tick they were removed from the world at.
    SavePlayer(Box<PlayerSnapshot>, Tick),
    MarkOnline(PlayerId),
    MarkOffline(PlayerId),
    ResetOnline,
    JournaledState(i32, oneshot::Sender<Option<CharacterSave>>),
    /// Replies once a world save taken now has been acknowledged.
    SaveWorld(oneshot::Sender<()>),
    /// Replies once every online event has been delivered and no world save is pending.
    Drain(oneshot::Sender<()>),
}

#[derive(Clone, Debug)]
pub struct PersistenceActorHandle {
    tx: mpsc::UnboundedSender<PersistenceCommand>,
}

impl PersistenceActorHandle {
    pub fn save_player(&self, player: Box<PlayerSnapshot>, tick: Tick) {
        let _ = self.tx.send(PersistenceCommand::SavePlayer(player, tick));
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

    pub async fn journaled_state(&self, character_id: i32) -> Option<CharacterSave> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(PersistenceCommand::JournaledState(character_id, reply))
            .ok()?;
        answer.await.ok().flatten()
    }

    pub async fn save_world(&self) {
        let (reply, answer) = oneshot::channel();
        if self.tx.send(PersistenceCommand::SaveWorld(reply)).is_ok() {
            let _ = answer.await;
        }
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

    /// A handle whose courier has nothing journaled and acknowledges everything at once.
    #[cfg(test)]
    pub fn nothing_pending() -> Self {
        let (handle, mut rx) = Self::for_test();
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    PersistenceCommand::JournaledState(_, reply) => {
                        let _ = reply.send(None);
                    }
                    PersistenceCommand::SaveWorld(reply) | PersistenceCommand::Drain(reply) => {
                        let _ = reply.send(());
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
    shared_map: Arc<ArcSwap<GameMap>>,
    save_every: Duration,
    players: BTreeMap<i32, JournaledPlayer>,
    world: Option<(WorldSave, Instant)>,
    acknowledged: Tick,
    online: VecDeque<OnlineEvent>,
    save_waiters: Vec<oneshot::Sender<()>>,
    drain_waiters: Vec<oneshot::Sender<()>>,
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl PersistenceActor {
    pub fn start(
        site: Arc<SiteClient>,
        journal: Journal,
        shared_map: Arc<ArcSwap<GameMap>>,
        save_every: Duration,
    ) -> PersistenceActorHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Self {
            rx,
            site,
            journal,
            shared_map,
            save_every,
            players: BTreeMap::new(),
            world: None,
            acknowledged: Tick(0),
            online: VecDeque::new(),
            save_waiters: Vec::new(),
            drain_waiters: Vec::new(),
            retry_at: None,
            backoff: FIRST_RETRY,
        };
        tokio::spawn(actor.run());
        PersistenceActorHandle { tx }
    }

    async fn run(mut self) {
        info!("Persistence actor started");
        let mut timer = interval_at(Instant::now() + self.save_every, self.save_every);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let retry_at = self.retry_at;
            tokio::select! {
                command = self.rx.recv() => match command {
                    Some(command) => self.handle(command).await,
                    None => break,
                },
                _ = timer.tick() => self.take_world_save().await,
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
            PersistenceCommand::SavePlayer(snapshot, tick) => {
                let player = JournaledPlayer {
                    tick: tick.0,
                    save: to_character_save(&snapshot),
                };
                if let Err(e) = self.journal.write_player(&player) {
                    error!(
                        character_id = player.save.id,
                        "Could not journal a logout; it is held in memory until the next world save: {e}"
                    );
                    telemetry::metrics().record_journal_failure();
                }
                self.players.insert(player.save.id, player);
                telemetry::metrics().record_saves_pending(self.players.len() as u64);
            }
            PersistenceCommand::MarkOnline(id) => {
                self.queue_online(OnlineEvent::Online(id.0 as i32)).await
            }
            PersistenceCommand::MarkOffline(id) => {
                self.queue_online(OnlineEvent::Offline(id.0 as i32)).await
            }
            PersistenceCommand::ResetOnline => self.queue_online(OnlineEvent::Reset).await,
            PersistenceCommand::JournaledState(id, reply) => {
                let _ = reply.send(self.players.get(&id).map(|player| player.save.clone()));
            }
            PersistenceCommand::SaveWorld(reply) => {
                self.save_waiters.push(reply);
                self.take_world_save().await;
            }
            PersistenceCommand::Drain(reply) => {
                self.drain_waiters.push(reply);
                self.deliver().await;
            }
        }
    }

    async fn queue_online(&mut self, event: OnlineEvent) {
        self.online.push_back(event);
        if self.retry_at.is_none() {
            self.deliver().await;
        }
    }

    async fn take_world_save(&mut self) {
        let snapshot = self.shared_map.load_full();
        let tick = snapshot.tick();
        let mut present = HashSet::new();
        let mut characters = Vec::new();
        for (key, agent) in snapshot.iter_agents() {
            let Some(position) = snapshot.agent_position(key) else {
                continue;
            };
            if let Some(player) = agent.to_snapshot(position.clone()) {
                present.insert(player.id.0 as i32);
                characters.push(to_character_save(&player));
            }
        }
        characters.extend(
            self.players
                .values()
                .filter(|player| player.tick <= tick.0 && !present.contains(&player.save.id))
                .map(|player| player.save.clone()),
        );
        let save = WorldSave {
            tick: tick.0,
            characters,
            chunks: chunk_rows(&snapshot, self.acknowledged),
        };
        if let Err(e) = self.journal.write_world(&save) {
            error!(
                tick = save.tick,
                "Could not journal the world save; sending it from memory: {e}"
            );
            telemetry::metrics().record_journal_failure();
        }
        self.world = Some((save, Instant::now()));
        self.deliver().await;
    }

    async fn deliver(&mut self) {
        let delivered = self.send_online().await && self.send_world().await;
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

    async fn send_world(&mut self) -> bool {
        let Some((save, _)) = &self.world else {
            return true;
        };
        let result = match self.site.world_save(save).await {
            Ok(result) => result,
            Err(e) => {
                error!(
                    tick = save.tick,
                    "Could not deliver the world save, retrying: {e}"
                );
                return false;
            }
        };
        let Some((save, taken_at)) = self.world.take() else {
            return true;
        };
        for id in result.skipped {
            warn!(
                character_id = id,
                "The site no longer has this character; its state was dropped"
            );
        }
        telemetry::metrics().record_world_save(taken_at.elapsed(), save.chunks.len() as u64);
        info!(
            tick = save.tick,
            characters = save.characters.len(),
            chunks = save.chunks.len(),
            "World saved"
        );
        self.acknowledged = Tick(save.tick);
        if let Err(e) = self.journal.remove_world() {
            error!("Could not remove the delivered world save from the journal: {e}");
        }
        let covered: Vec<i32> = self
            .players
            .iter()
            .filter(|(_, player)| player.tick <= save.tick)
            .map(|(id, _)| *id)
            .collect();
        for id in covered {
            self.players.remove(&id);
            if let Err(e) = self.journal.remove_player_up_to(id, save.tick) {
                error!(
                    character_id = id,
                    "Could not remove a saved logout from the journal: {e}"
                );
            }
        }
        telemetry::metrics().record_saves_pending(self.players.len() as u64);
        for waiter in self.save_waiters.drain(..) {
            let _ = waiter.send(());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::items::{Item, ItemConfig, ItemFlag, ItemId};
    use crate::entities::map::MapTile;
    use crate::entities::position::Position;
    use crate::entities::world_map::WorldMap;
    use crate::persistence::test_fixtures::a_test_snapshot;

    async fn a_site(status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({ "skipped": [] })))
            .mount(&server)
            .await;
        server
    }

    fn a_courier(
        server: &MockServer,
        dir: &std::path::Path,
        map: GameMap,
    ) -> PersistenceActorHandle {
        PersistenceActor::start(
            Arc::new(SiteClient::new(&server.uri(), reqwest::Client::new())),
            Journal::open(dir).unwrap(),
            Arc::new(ArcSwap::from_pointee(map)),
            Duration::from_secs(3600),
        )
    }

    async fn world_saves(server: &MockServer) -> Vec<WorldSave> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/internal/world-saves")
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    fn logout(id: u32) -> Box<PlayerSnapshot> {
        Box::new(a_test_snapshot(id, 1))
    }

    /// A map at tick 10 with player 1 on it and one item written at tick 10.
    fn a_map_with_player_1() -> GameMap {
        let pos = Position::new(20, 20, 7);
        let mut game = GameMap::new();
        game.insert_tile(pos.clone(), MapTile::new());
        game.insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();
        let mut map = WorldMap::new(game);
        map.begin_tick(Tick(10));
        let coin = Arc::new(ItemConfig::new(
            ItemId(3031),
            "coin".to_string(),
            None,
            None,
            [ItemFlag::Take],
            Vec::new(),
        ));
        map.place_item(&pos, None, None, Item::new(coin, 1))
            .unwrap();
        map.snapshot()
    }

    fn ids(save: &WorldSave) -> Vec<i32> {
        let mut ids: Vec<i32> = save.characters.iter().map(|c| c.id).collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn a_logout_is_journaled_and_not_sent() {
        let server = a_site(200).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), GameMap::new());

        courier.save_player(logout(2), Tick(5));

        assert!(courier.journaled_state(2).await.is_some());
        assert_eq!(
            Journal::open(dir.path()).unwrap().players().unwrap().len(),
            1
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_world_save_takes_the_snapshot_players_and_logouts_up_to_its_tick() {
        let server = a_site(200).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), a_map_with_player_1());
        courier.save_player(logout(2), Tick(9));
        courier.save_player(logout(3), Tick(11));

        courier.save_world().await;

        let saves = world_saves(&server).await;
        assert_eq!(saves.len(), 1);
        assert_eq!(saves[0].tick, 10);
        assert_eq!(ids(&saves[0]), vec![1, 2]);
        assert_eq!(saves[0].chunks.len(), 1);
        assert!(courier.journaled_state(2).await.is_none());
        assert!(courier.journaled_state(3).await.is_some());
        let left: Vec<i32> = Journal::open(dir.path())
            .unwrap()
            .players()
            .unwrap()
            .iter()
            .map(|p| p.save.id)
            .collect();
        assert_eq!(left, vec![3]);
        assert_eq!(Journal::open(dir.path()).unwrap().world().unwrap(), None);
    }

    #[tokio::test]
    async fn a_journaled_player_still_in_the_snapshot_is_saved_from_the_snapshot() {
        let server = a_site(200).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), a_map_with_player_1());
        let mut stale = a_test_snapshot(1, 1);
        stale.life.current = 1;
        courier.save_player(Box::new(stale), Tick(3));

        courier.save_world().await;

        let saves = world_saves(&server).await;
        assert_eq!(ids(&saves[0]), vec![1]);
        assert_ne!(saves[0].characters[0].life.current, 1);
    }

    #[tokio::test]
    async fn a_failed_world_save_stays_journaled_and_a_chunk_is_not_forgotten() {
        let server = a_site(500).await;
        let dir = tempfile::tempdir().unwrap();
        let courier = a_courier(&server, dir.path(), a_map_with_player_1());

        let (reply, _answer) = oneshot::channel();
        courier
            .tx
            .send(PersistenceCommand::SaveWorld(reply))
            .unwrap();
        courier.save_player(logout(2), Tick(1));

        assert!(courier.journaled_state(2).await.is_some());

        let journaled = Journal::open(dir.path()).unwrap().world().unwrap().unwrap();
        assert_eq!(journaled.chunks.len(), 1);
    }
}
