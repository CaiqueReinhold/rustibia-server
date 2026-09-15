//! What the client currently knows about: viewport descriptions, the agent
//! id map and its recycling, spawn/despawn, and the current target.

use anyhow::Result;
use tracing::error;

use crate::actors::player_query::{
    get_agent_desc, get_player_desc, get_player_skills, get_spell_list,
};
use crate::actors::session::{SessionActor, SessionError};
use crate::constants::view::AGENT_DESPAWN_RADIUS;
use crate::entities::agent::AgentKey;
use crate::entities::effects::MissileId;
use crate::entities::map::GameMap;
use crate::entities::position::{Position, Rect};
use crate::entities::skills::SkillType;
use crate::game::config::GAME_CONFIG;
use crate::game::map_query::get_agents_in_viewport;
use crate::game::map_query::get_map_desc_on_viewport;
use crate::game::skills::{progress_bp, total_experience};
use crate::messages::{FloatingTextType, ServerMessage, SkillProgress, TextMessageType};
use crate::persistence::player::PlayerSnapshot;

impl SessionActor {
    pub(super) async fn player_spawned(
        &mut self,
        agent_key: AgentKey,
        position: Position,
    ) -> Result<()> {
        let map = self.shared_map.load();
        if self.player_key == agent_key {
            let self_id = self.agents.get_or_insert(self.player_key);

            self.send_map_description(&position, &map).await?;
            self.send_agents_description(&position, &map).await?;

            let player_desc = get_player_desc(&map, self.player_key, self_id);
            if let Some(pdesc_msg) = player_desc {
                self.connection.send_message(pdesc_msg).await?;
            } else {
                return Err(SessionError::FailedToInitialize.into());
            }

            if let Some(skills_msg) = get_player_skills(&map, self.player_key) {
                self.connection.send_message(skills_msg).await?;
            }

            if let Some(spells_msg) = get_spell_list(&map, self.player_key) {
                self.connection.send_message(spells_msg).await?;
            }

            Ok(())
        } else {
            self.introduce_agent(agent_key, position, &map).await
        }
    }

    pub(super) async fn send_agents_description(
        &mut self,
        position: &Position,
        map: &GameMap,
    ) -> Result<()> {
        for (key, pos) in get_agents_in_viewport(map, position) {
            if key == self.player_key {
                continue;
            }
            if self.agents.get_local(&key).is_none() {
                self.introduce_agent(key, pos, map).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn send_map_description(
        &self,
        position: &Position,
        map: &GameMap,
    ) -> Result<()> {
        let map_desc_floors = get_map_desc_on_viewport(map, position);
        for (floor, tiles) in map_desc_floors {
            self.connection
                .send_message(ServerMessage::DescribeMap {
                    tiles,
                    center: position.clone(),
                    floor,
                })
                .await?;
        }
        Ok(())
    }

    pub(super) async fn remove_agents_not_in_reach(&mut self) -> Result<()> {
        let map = self.shared_map.load();
        let Some(pos) = map.agent_position(self.player_key) else {
            return Err(SessionError::InvalidState.into());
        };
        let gone: Vec<AgentKey> = self
            .agents
            .iter_global()
            .filter(|key| {
                **key != self.player_key
                    && map.agent_position(**key).is_some_and(|agent_pos| {
                        !Rect::radius(pos, AGENT_DESPAWN_RADIUS).contains(agent_pos)
                            || pos.z.abs_diff(agent_pos.z) > 2
                    })
            })
            .copied()
            .collect();

        for key in gone {
            self.forget_agent(key).await?;
        }

        Ok(())
    }

    pub(super) async fn introduce_agent(
        &mut self,
        agent_key: AgentKey,
        position: Position,
        map: &GameMap,
    ) -> Result<()> {
        let Some(agent) = map.get_agent(agent_key) else {
            return Ok(());
        };
        if self.agents.get_local(&agent_key).is_some() {
            return Ok(());
        }
        let agent_id = self.agents.get_or_insert(agent_key);
        self.connection
            .send_message(get_agent_desc(agent, agent_id, position))
            .await?;
        Ok(())
    }

    pub(super) async fn forget_agent(&mut self, agent_key: AgentKey) -> Result<()> {
        let Some(agent_id) = self.agents.get_local(&agent_key) else {
            return Ok(());
        };

        self.agents.remove_by_local(agent_id);
        self.connection
            .send_message(ServerMessage::RemoveAgent { agent_id })
            .await?;

        Ok(())
    }

    pub(super) async fn agent_despawned(
        &mut self,
        agent_key: AgentKey,
        snapshot: Option<Box<PlayerSnapshot>>,
    ) -> Result<()> {
        if self.player_key == agent_key {
            if let Some(snapshot) = snapshot {
                if let Err(e) = self.persistence.save_player(snapshot).await {
                    error!(
                        session = self.session_id,
                        "Failed to save player on logout: {e}"
                    );
                }
                return Err(SessionError::Logout.into());
            }
            return Ok(());
        }

        self.forget_agent(agent_key).await?;
        Ok(())
    }

    pub(super) async fn life_updated(&self, agent_key: AgentKey) -> Result<()> {
        let map = self.shared_map.load();
        let Some(agent) = map.get_agent(agent_key) else {
            return Ok(());
        };
        let Some(agent_id) = self.agents.get_local(&agent_key) else {
            return Ok(());
        };
        let (current, max) = if agent_key == self.player_key {
            (agent.life().current, agent.life().maximum)
        } else {
            (agent.life().to_wire(), 100)
        };
        self.connection
            .send_message(ServerMessage::AgentLifeChanged {
                agent_id,
                current,
                max,
            })
            .await?;
        Ok(())
    }

    pub(super) async fn mana_updated(&self) -> Result<()> {
        let map = self.shared_map.load();
        if let Some(agent) = map.get_agent(self.player_key)
            && let Some(agent_id) = self.agents.get_local(&self.player_key)
        {
            self.connection
                .send_message(ServerMessage::AgentManaUpdated {
                    agent_id,
                    current: agent.mana().current,
                    max: agent.mana().maximum,
                })
                .await?;
        }
        Ok(())
    }

    pub(super) async fn skill_progress(&self, skill: SkillType, amount: u64) -> Result<()> {
        self.send_skill_update(skill, amount).await
    }

    async fn send_skill_update(&self, skill: SkillType, amount: u64) -> Result<()> {
        let (progress, experience, position) = {
            let map = self.shared_map.load();
            let Some(player) = map.get_player(self.player_key) else {
                return Ok(());
            };
            let Some(value) = player.skills().get(&skill) else {
                return Ok(());
            };
            (
                SkillProgress {
                    level: value.value,
                    percent_bp: progress_bp(player.vocation(), &skill, value),
                },
                (skill == SkillType::Level).then(|| total_experience(value)),
                map.agent_position(self.player_key).cloned(),
            )
        };

        self.connection
            .send_message(ServerMessage::SkillUpdated { skill, progress })
            .await?;

        if let Some(experience) = experience {
            if let Some(position) = position {
                self.connection
                    .send_message(ServerMessage::FloatingText {
                        text: amount.to_string(),
                        position,
                        text_type: FloatingTextType::HitPoints,
                        color: Some(GAME_CONFIG.text_colors.white),
                    })
                    .await?;
            }
            self.connection
                .send_message(ServerMessage::ExperienceUpdated { experience })
                .await?;
        }
        Ok(())
    }

    pub(super) async fn skill_upgraded(
        &self,
        skill: SkillType,
        gained: u16,
        amount: u64,
    ) -> Result<()> {
        self.send_skill_update(skill, amount).await?;
        let map = self.shared_map.load();
        let message = map.get_player(self.player_key).map(|p| {
            let value = p.skill(skill);
            match skill {
                SkillType::Axe => format!("You advanced to axe fighting {value}"),
                SkillType::Club => format!("You advanced to club fighting {value}"),
                SkillType::Sword => format!("You advanced to sword fighting {value}"),
                SkillType::Distance => format!("You advanced to distance fighting {value}"),
                SkillType::Magic => format!("You advanced to magic level {value}"),
                SkillType::Shielding => format!("You advanced to shielding {value}"),
                SkillType::Level => format!(
                    "You advanced from level {} to level {value}",
                    value.saturating_sub(gained)
                ),
            }
        });

        if let Some(message) = message {
            self.connection
                .send_message(ServerMessage::TextMessage {
                    text: message,
                    message_type: TextMessageType::Look,
                })
                .await?;
        }

        if matches!(skill, SkillType::Level)
            && let Some(agent) = map.get_agent(self.player_key)
            && let Some(player) = agent.get_player()
        {
            let agent_id = self
                .agents
                .get_local(&self.player_key)
                .ok_or(SessionError::InvalidState)?;
            self.connection
                .send_message(ServerMessage::AgentLifeChanged {
                    agent_id,
                    current: agent.life().current,
                    max: agent.life().maximum,
                })
                .await?;
            self.connection
                .send_message(ServerMessage::AgentManaUpdated {
                    agent_id,
                    current: agent.mana().current,
                    max: agent.mana().maximum,
                })
                .await?;
            self.connection
                .send_message(ServerMessage::PlayerCapacityUpdated {
                    cap: player.capacity_available(),
                })
                .await?;
        }
        Ok(())
    }

    pub(super) async fn missile_launched(
        &self,
        from: Position,
        to: Position,
        missile_id: MissileId,
    ) -> Result<()> {
        self.connection
            .send_message(ServerMessage::LaunchMissile {
                from,
                to,
                missile_id,
            })
            .await?;
        Ok(())
    }

    pub(super) async fn agent_speed_changed(&self, agent_key: AgentKey) -> Result<()> {
        let speed = {
            let map = self.shared_map.load();
            map.get_agent(agent_key).map(|a| a.speed())
        };
        if let Some(speed) = speed
            && let Some(agent_id) = self.agents.get_local(&agent_key)
        {
            self.connection
                .send_message(ServerMessage::AgentSpeedUpdated { agent_id, speed })
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::connection::ConnectionCommand;
    use crate::actors::session::test_support::seat_player;
    use crate::actors::world::WorldCommand;
    use crate::entities::agent::AgentId;
    use crate::entities::agent::AgentKey;
    use crate::entities::map::GameMap;
    use crate::entities::position::Position;
    use crate::entities::skills::SkillValue;
    use crate::game::TickDelta;
    use crate::messages::ServerMessage;
    use tokio::sync::mpsc;

    #[tokio::test]
    pub(super) async fn target_lost_forwards_the_seq_verbatim() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session.target_lost(77).await.unwrap();

        assert!(matches!(
            connection_rx.try_recv(),
            Ok(ConnectionCommand::SendPlayerMessage(
                ServerMessage::TargetLost { seq: 77 }
            ))
        ));
    }

    #[tokio::test]
    pub(super) async fn set_target_forwards_the_seq_to_the_world() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let victim = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let (mut session, _connection_rx, mut world_rx, _tick_tx) = SessionActor::for_test(me, map);
        let local = session.agents.get_or_insert(victim);

        session.handle_set_target(Some(local), 12).await.unwrap();

        let (cmd, _) = world_rx.try_recv().unwrap();
        assert!(matches!(
            cmd,
            WorldCommand::SetTarget { target: Some(t), seq: 12, .. } if t == victim
        ));
    }

    #[tokio::test]
    pub(super) async fn set_target_translates_the_local_id_to_a_key() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let victim = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let (mut session, _connection_rx, mut world_rx, _tick_tx) = SessionActor::for_test(me, map);
        let local = session.agents.get_or_insert(victim);

        session.handle_set_target(Some(local), 0).await.unwrap();

        let (cmd, _) = world_rx.try_recv().unwrap();
        assert!(matches!(
            cmd,
            WorldCommand::SetTarget { target: Some(t), .. } if t == victim
        ));
    }

    /// The agent has already left view and the client is a tick behind. The honest
    /// answer is "you have no target", not a dropped message.
    #[tokio::test]
    pub(super) async fn an_unknown_local_id_becomes_a_clear() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, _connection_rx, mut world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session
            .handle_set_target(Some(AgentId(4242)), 0)
            .await
            .unwrap();

        let (cmd, _) = world_rx.try_recv().unwrap();
        assert!(matches!(cmd, WorldCommand::SetTarget { target: None, .. }));
    }

    /// Seats a player, a victim it is already targeting (map state kept for
    /// narrative clarity — `forget_agent` no longer reads it; the compare is the
    /// world's job now), and an unrelated bystander.
    #[allow(clippy::type_complexity)]
    pub(super) fn a_session_with_a_target() -> (
        SessionActor,
        AgentKey,
        AgentKey,
        mpsc::Receiver<ConnectionCommand>,
        mpsc::Receiver<(WorldCommand, Option<TickDelta>)>,
    ) {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let victim = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let bystander = seat_player(&mut map, &Position::new(102, 100, 7), 3);
        map.get_agent_mut(me).unwrap().set_target(Some(victim), 0);

        let (session, connection_rx, world_rx, _tick_tx) = SessionActor::for_test(me, map);
        (session, victim, bystander, connection_rx, world_rx)
    }

    /// An agent that was never introduced has no id to drop and nothing to announce.
    #[tokio::test]
    pub(super) async fn forgetting_an_unknown_agent_does_nothing() {
        let (mut session, victim, _bystander, mut connection_rx, _world_rx) =
            a_session_with_a_target();
        // `victim` is the target but was never introduced, so it has no local id.

        session.forget_agent(victim).await.unwrap();

        assert!(connection_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_ticked_skill_sends_its_level_and_progress() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let sword = SkillValue {
            value: 11,
            current_ticks: 27,
        };
        let player = map.get_player_mut(me).unwrap();
        let sword_bp = progress_bp(player.vocation(), &SkillType::Sword, &sword);
        player.skills_mut().insert(SkillType::Sword, sword);
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session.skill_progress(SkillType::Sword, 1).await.unwrap();

        assert!(matches!(
            connection_rx.try_recv(),
            Ok(ConnectionCommand::SendPlayerMessage(
                ServerMessage::SkillUpdated {
                    skill: SkillType::Sword,
                    progress,
                }
            )) if progress.level == 11 && progress.percent_bp == sword_bp
        ));
    }

    /// Experience is its own message, so a client that only wants the total does
    /// not have to know that `Level` is a skill.
    #[tokio::test]
    async fn the_level_skill_also_sends_the_experience_total() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        map.get_player_mut(me).unwrap().skills_mut().insert(
            SkillType::Level,
            SkillValue {
                value: 8,
                current_ticks: 55,
            },
        );
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session.skill_progress(SkillType::Level, 30).await.unwrap();

        assert!(matches!(
            connection_rx.try_recv(),
            Ok(ConnectionCommand::SendPlayerMessage(
                ServerMessage::SkillUpdated { .. }
            ))
        ));
        // The floating number rides between the two, and now goes out whether or
        // not this session has a local id for the player: it is pinned to a tile,
        // and nothing about it is addressed to an agent.
        assert!(matches!(
            connection_rx.try_recv(),
            Ok(ConnectionCommand::SendPlayerMessage(
                ServerMessage::FloatingText { .. }
            ))
        ));
        assert!(matches!(
            connection_rx.try_recv(),
            Ok(ConnectionCommand::SendPlayerMessage(
                ServerMessage::ExperienceUpdated { experience: 4255 }
            ))
        ));
    }

    /// The floating number over a player that just levelled is the experience the kill
    /// awarded. It used to be a hard-coded `0`, because `SkillUpgraded` did not carry it.
    #[tokio::test]
    async fn a_level_up_floats_the_experience_it_awarded() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        map.get_player_mut(me).unwrap().skills_mut().insert(
            SkillType::Level,
            SkillValue {
                value: 2,
                current_ticks: 0,
            },
        );
        let (mut session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);
        session.agents.get_or_insert(me);

        session
            .skill_upgraded(SkillType::Level, 1, 40)
            .await
            .unwrap();

        let mut floated = None;
        while let Ok(ConnectionCommand::SendPlayerMessage(message)) = connection_rx.try_recv() {
            if let ServerMessage::FloatingText { text, .. } = message {
                floated = Some(text);
            }
        }
        assert_eq!(floated.as_deref(), Some("40"));
    }

    /// `tick_skill` returns without emitting for a skill the player has no row
    /// for, but the broadcast handler must not assume that — the map it reads is
    /// a snapshot, not the map the event was produced from.
    #[tokio::test]
    async fn a_skill_the_player_does_not_have_sends_nothing() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        map.get_player_mut(me).unwrap().skills_mut().clear();
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session.skill_progress(SkillType::Axe, 1).await.unwrap();

        assert!(connection_rx.try_recv().is_err());
    }
}
