use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{error, info};

use crate::entities::player::PlayerId;
use crate::persistence::online::OnlineRepository;
use crate::persistence::player::{PlayerRepository, PlayerSnapshot};

#[derive(Clone, Debug)]
pub enum PersistenceCommand {
    SavePlayer(Box<PlayerSnapshot>),
    MarkOnline(PlayerId),
    MarkOffline(PlayerId),
}

#[derive(Clone, Debug)]
pub struct PersistenceActorHandle {
    tx: mpsc::UnboundedSender<PersistenceCommand>,
}

impl PersistenceActorHandle {
    pub fn save_player(&self, player: Box<PlayerSnapshot>) {
        // If this actor stops world stops too, so error can be swallowed
        let _ = self.tx.send(PersistenceCommand::SavePlayer(player));
    }

    pub fn mark_online(&self, character_id: PlayerId) {
        if self
            .tx
            .send(PersistenceCommand::MarkOnline(character_id))
            .is_err()
        {
            tracing::warn!(
                character_id = %character_id,
                "dropped online marker: persistence channel full"
            );
        }
    }

    pub fn mark_offline(&self, character_id: PlayerId) {
        if self
            .tx
            .send(PersistenceCommand::MarkOffline(character_id))
            .is_err()
        {
            tracing::warn!(
                character_id = %character_id,
                "dropped offline marker: persistence channel full"
            );
        }
    }

    /// A handle with no actor behind it, plus the receiving end so a test can assert
    /// what was sent. Exists because `OnlineRegistry` now requires a handle, and
    /// spinning up a real `PersistenceActor` would drag a database into what are
    /// otherwise pure in-memory tests.
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::UnboundedReceiver<PersistenceCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (PersistenceActorHandle { tx }, rx)
    }
}

pub struct PersistenceActor {
    rx: mpsc::UnboundedReceiver<PersistenceCommand>,
    repo: Arc<PlayerRepository>,
    online: Arc<OnlineRepository>,
}

impl PersistenceActor {
    pub fn start(
        repo: Arc<PlayerRepository>,
        online: Arc<OnlineRepository>,
    ) -> PersistenceActorHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let actor = Self { rx, repo, online };
            actor.run().await;
        });
        PersistenceActorHandle { tx }
    }

    async fn run(mut self) {
        info!("Persistence actor started");
        while let Some(cmd) = self.rx.recv().await {
            match cmd {
                PersistenceCommand::SavePlayer(snapshot) => {
                    let player_id = snapshot.id;
                    if let Err(e) = self.repo.save(&snapshot).await {
                        error!(player_id = %player_id, "Failed to save player: {e}");
                    }
                }
                PersistenceCommand::MarkOnline(character_id) => {
                    if let Err(e) = self.online.mark_online(character_id).await {
                        error!(character_id = %character_id, "Failed to mark player online: {e}");
                    }
                }
                PersistenceCommand::MarkOffline(character_id) => {
                    if let Err(e) = self.online.mark_offline(character_id).await {
                        error!(character_id = %character_id, "Failed to mark player offline: {e}");
                    }
                }
            }
        }
        info!("Persistence actor stopped");
    }
}
