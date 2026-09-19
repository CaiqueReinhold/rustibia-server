//! Movement: the player's walk queue and its tick check, plus the position
//! events pushed back to the client.

use anyhow::Result;

use crate::actors::session::{SessionActor, SessionError};
use crate::actors::world::WorldCommand;
use crate::entities::agent::Facing;
use crate::entities::agent::{AgentId, AgentKey};
use crate::entities::position::{Direction, Position};
use crate::game::TickDelta;
use crate::game::map_query::get_map_expansion;
use crate::game::map_query::{get_agents_in_expansion, iter_visible_floors};
use crate::messages::ServerMessage;

impl SessionActor {
    pub(super) fn walk_cooldown_remaining(&self) -> TickDelta {
        let map = self.shared_map.load();
        map.get_agent(self.player_key)
            .map(|agent| agent.next_walk_tick.saturating_sub(*self.tick_rx.borrow()))
            .unwrap_or(TickDelta(0))
    }

    pub(super) async fn check_walk_queue(&mut self) -> Result<()> {
        let Some(direction) = self.queued_walk else {
            return Ok(());
        };
        if self.walk_cooldown_remaining() > TickDelta(0) {
            return Ok(());
        }
        self.queued_walk = None;
        self.send_walk(direction).await
    }

    pub(super) async fn send_walk(&self, direction: Direction) -> Result<()> {
        self.world
            .send(WorldCommand::Walk {
                direction,
                agent_key: self.player_key,
            })
            .await;
        Ok(())
    }

    pub(super) async fn handle_move_player(&mut self, direction: Direction) -> Result<()> {
        let remaining = self.walk_cooldown_remaining();
        if remaining > TickDelta(0) {
            self.queued_walk = Some(direction);
            return Ok(());
        }
        self.queued_walk = None;
        self.send_walk(direction).await
    }

    pub(super) async fn handle_get_position(&self) -> Result<()> {
        let map = self.shared_map.load();
        if let Some(position) = map.agent_position(self.player_key) {
            self.connection
                .send_message(ServerMessage::PlayerPosition {
                    position: position.clone(),
                })
                .await?;
        } else {
            return Err(SessionError::WrongMessageType.into());
        }
        Ok(())
    }

    pub(super) async fn handle_change_direction(&self, facing: Facing) -> Result<()> {
        self.world
            .send(WorldCommand::ChangeDirection {
                agent: self.player_key,
                facing,
            })
            .await;
        Ok(())
    }

    pub(super) async fn agent_moved(
        &mut self,
        agent_key: AgentKey,
        direction: Direction,
        to_position: Position,
    ) -> Result<()> {
        let map = self.shared_map.load();
        if self.player_key == agent_key {
            self.drop_unreachable_containers().await?;

            for (key, pos) in get_agents_in_expansion(&map, &to_position, &direction) {
                self.introduce_agent(key, pos, &map).await?;
            }

            let tiles = {
                let from_pos = to_position.clone() - direction;
                get_map_expansion(&map, &from_pos, &direction)
            };
            self.connection
                .send_message(ServerMessage::PlayerWalkAck {
                    position: to_position.clone(),
                    tiles,
                })
                .await?;

            Ok(())
        } else {
            if let Some(agent_id) = self.agents.get_local(&agent_key) {
                let from = to_position.clone() - direction;
                self.connection
                    .send_message(ServerMessage::MoveAgent {
                        agent_id,
                        direction,
                        from,
                    })
                    .await?;
            } else {
                self.introduce_agent(agent_key, to_position, &map).await?;
            }

            Ok(())
        }
    }

    pub(super) async fn walk_denied(&self) -> Result<()> {
        self.connection
            .send_message(ServerMessage::PlayerWalkDenied)
            .await?;

        Ok(())
    }

    pub(super) async fn agent_teleported(
        &mut self,
        agent_key: AgentKey,
        to_position: Position,
    ) -> Result<()> {
        let map = self.shared_map.load();
        if agent_key == self.player_key {
            self.send_map_description(&to_position, &map).await?;
            self.send_agents_description(&to_position, &map).await?;

            let self_id = self
                .agents
                .get_local(&self.player_key)
                .ok_or(SessionError::InvalidState)?;
            self.connection
                .send_message(ServerMessage::TeleportAgent {
                    agent_id: self_id,
                    position: to_position.clone(),
                })
                .await?;

            Ok(())
        } else {
            let Some(my_pos) = map.agent_position(self.player_key) else {
                return Ok(());
            };

            if iter_visible_floors(my_pos.z)
                .find(|z| *z == to_position.z)
                .is_some()
            {
                if let Some(agent_id) = self.agents.get_local(&agent_key) {
                    self.connection
                        .send_message(ServerMessage::TeleportAgent {
                            agent_id,
                            position: to_position,
                        })
                        .await?;
                } else {
                    self.introduce_agent(agent_key, to_position, &map).await?;
                }
            }

            Ok(())
        }
    }

    pub(super) async fn facing_changed(
        &self,
        agent_key: AgentKey,
        agent_id: AgentId,
    ) -> Result<()> {
        let facing = self
            .shared_map
            .load()
            .get_agent(agent_key)
            .map(|agent| agent.facing());
        if let Some(facing) = facing {
            self.connection
                .send_message(ServerMessage::AgentChangedDirection { agent_id, facing })
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::session::test_support::seat_player;
    use crate::actors::world::WorldCommand;
    use crate::entities::map::GameMap;
    use crate::entities::position::Position;
    use crate::game::Tick;
    use std::sync::Arc;

    /// An arbitrary future tick to arm the cooldown to. Nothing depends on the exact
    /// value now that the queue has no window — only that it is ahead of the clock.
    const COOLDOWN_TICKS: Tick = Tick(4);

    /// Puts the seated player on cooldown as though it had just walked.
    pub(super) fn arm_cooldown(session: &SessionActor, until: Tick) {
        let mut map = (**session.shared_map.load()).clone();
        map.get_agent_mut(session.player_key)
            .unwrap()
            .next_walk_tick = until;
        session.shared_map.store(Arc::new(map));
    }

    #[tokio::test]
    pub(super) async fn a_walk_with_no_cooldown_is_forwarded_immediately() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, _connection_rx, mut world_rx, _tick_tx) =
            SessionActor::for_test(key, map);

        session.handle_move_player(Direction::North).await.unwrap();

        assert!(
            matches!(world_rx.try_recv(), Ok((WorldCommand::Walk { .. }, _))),
            "nothing to wait for, so the walk goes straight through"
        );
        assert!(session.queued_walk.is_none());
    }

    /// The whole point: a walk that arrives early is held, not refused.
    #[tokio::test]
    pub(super) async fn an_early_walk_is_held() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, _connection_rx, mut world_rx, _tick_tx) =
            SessionActor::for_test(key, map);
        arm_cooldown(&session, COOLDOWN_TICKS);

        session.handle_move_player(Direction::North).await.unwrap();

        assert!(
            world_rx.try_recv().is_err(),
            "an early walk must not reach the world, where it would be denied"
        );
        assert!(
            matches!(session.queued_walk, Some(Direction::North)),
            "it is held instead"
        );
    }

    #[tokio::test]
    pub(super) async fn a_held_walk_is_forwarded_when_the_cooldown_expires() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, _connection_rx, mut world_rx, tick_tx) = SessionActor::for_test(key, map);
        arm_cooldown(&session, COOLDOWN_TICKS);

        session.handle_move_player(Direction::North).await.unwrap();
        session.check_walk_queue().await.unwrap();
        assert!(world_rx.try_recv().is_err(), "still early, so still held");

        tick_tx.send(COOLDOWN_TICKS).unwrap();
        session.check_walk_queue().await.unwrap();

        assert!(
            matches!(world_rx.try_recv(), Ok((WorldCommand::Walk { .. }, _))),
            "the held walk goes through on the tick the cooldown expires"
        );
        assert!(session.queued_walk.is_none(), "and the slot is cleared");
    }

    /// Newest wins, and it is the only staleness bound the queue has: each request
    /// overwrites the slot, so what fires is the newest intent and at most one step
    /// can be stale. Without this a direction change would be dropped and the player
    /// would keep walking the abandoned way.
    #[tokio::test]
    pub(super) async fn a_second_walk_replaces_the_queued_one() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, _connection_rx, mut world_rx, tick_tx) = SessionActor::for_test(key, map);
        arm_cooldown(&session, COOLDOWN_TICKS);

        session.handle_move_player(Direction::North).await.unwrap();
        session.handle_move_player(Direction::East).await.unwrap();

        tick_tx.send(COOLDOWN_TICKS).unwrap();
        session.check_walk_queue().await.unwrap();

        assert!(
            matches!(
                world_rx.try_recv(),
                Ok((
                    WorldCommand::Walk {
                        direction: Direction::East,
                        ..
                    },
                    _
                ))
            ),
            "the newer direction is what fires"
        );
        assert!(
            world_rx.try_recv().is_err(),
            "and only one walk is forwarded, not two"
        );
    }

    /// The regression that removing the window guards against. A walk far ahead of
    /// the cooldown used to be forwarded so the world could refuse it, on the theory
    /// that only a desynced client could be that early. It is not — a held walk
    /// delays its own ack, which delays the client's next send by the same amount,
    /// so the offset between the two clocks persists and lands wherever the last
    /// perturbation put it. Any fixed bound therefore becomes a refusal the moment
    /// terrain changes the step cost, which is the failure the queue exists to stop.
    #[tokio::test]
    pub(super) async fn a_walk_far_ahead_of_the_cooldown_is_still_held() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, _connection_rx, mut world_rx, _tick_tx) =
            SessionActor::for_test(key, map);
        arm_cooldown(&session, Tick(COOLDOWN_TICKS.0 * 25));

        session.handle_move_player(Direction::North).await.unwrap();

        assert!(
            world_rx.try_recv().is_err(),
            "however early it is, it waits rather than being refused"
        );
        assert!(matches!(session.queued_walk, Some(Direction::North)));
    }
}
