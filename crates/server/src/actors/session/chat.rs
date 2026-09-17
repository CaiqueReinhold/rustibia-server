//! Chat: local speech, private messages, and channels. An author is named over the
//! wire by their character name — the one identifier both sides already agree on.

use anyhow::Result;

use crate::actors::session::SessionActor;
use crate::actors::world::WorldCommand;
use crate::entities::agent::AgentKey;
use crate::entities::chat::ChannelId;
use crate::entities::chat::ChatMessageType;
use crate::entities::chat::SayTarget;
use crate::entities::position::Position;
use crate::game::config::GAME_CONFIG;
use crate::messages::FloatingTextType;
use crate::messages::ServerMessage;

impl SessionActor {
    fn agent_name(&self, agent_key: AgentKey) -> Option<String> {
        let map = self.shared_map.load();
        map.get_agent(agent_key)
            .map(|agent| agent.name().to_owned())
    }

    fn online_player_by_name(&self, name: &str) -> Option<AgentKey> {
        let map = self.shared_map.load();
        map.iter_agents()
            .find(|(_, agent)| !agent.is_creature() && agent.name().eq_ignore_ascii_case(name))
            .map(|(key, _)| key)
    }

    pub(super) async fn action_message(&self, position: Position, message: String) -> Result<()> {
        self.connection
            .send_message(ServerMessage::FloatingText {
                text: message,
                position,
                text_type: FloatingTextType::CreatureSay,
                color: None,
            })
            .await?;
        Ok(())
    }

    pub(super) async fn send_chat(
        &mut self,
        author: AgentKey,
        message_type: ChatMessageType,
        channel: ChannelId,
        position: Option<Position>,
        message: String,
    ) -> Result<()> {
        let is_creature = {
            let map = self.shared_map.load();
            map.get_agent(author).is_some_and(|a| a.is_creature())
        };

        if matches!(message_type, ChatMessageType::Local) && is_creature {
            if let Some(position) = position {
                self.connection
                    .send_message(ServerMessage::FloatingText {
                        text: message.clone(),
                        position,
                        text_type: FloatingTextType::CreatureSay,
                        color: None,
                    })
                    .await?;
            }
            return Ok(());
        }

        let Some(author) = self.agent_name(author) else {
            return Ok(());
        };

        self.connection
            .send_message(ServerMessage::ChatMessage {
                author,
                message_type,
                channel,
                position: matches!(message_type, ChatMessageType::Local)
                    .then_some(position)
                    .flatten(),
                message,
            })
            .await?;

        Ok(())
    }

    pub(super) async fn receive_private_message(
        &mut self,
        author: AgentKey,
        message: String,
    ) -> Result<()> {
        self.send_chat(
            author,
            ChatMessageType::Private,
            ChannelId(0),
            None,
            message,
        )
        .await
    }

    pub(super) async fn receive_channel_message(
        &mut self,
        author: AgentKey,
        channel: ChannelId,
        message: String,
    ) -> Result<()> {
        self.send_chat(author, ChatMessageType::Channel, channel, None, message)
            .await
    }

    pub(super) async fn handle_say(&mut self, message: String, target: SayTarget) -> Result<()> {
        if message.chars().count() > GAME_CONFIG.chat.max_message_length {
            return self.deny("Your message is too long.").await;
        }

        let now = *self.tick_rx.borrow();
        if now < self.next_chat_tick {
            return self.deny("You are sending messages too fast.").await;
        }
        self.next_chat_tick = now + GAME_CONFIG.chat.message_cooldown_ticks;

        match target {
            SayTarget::Local => {
                self.world
                    .send(WorldCommand::Say {
                        agent_key: self.player_key,
                        message,
                    })
                    .await;
            }
            SayTarget::Player(name) => {
                let Some(recipient) = self.online_player_by_name(&name) else {
                    return self.deny("A player with this name is not online.").await;
                };
                self.chat
                    .message_player(self.player_key, recipient, message)
                    .await;
            }
            SayTarget::Channel(channel) => {
                self.chat
                    .message_channel(self.player_key, channel, message)
                    .await;
            }
        }
        Ok(())
    }

    pub(super) async fn handle_open_pm_chat(&mut self, name: String) -> Result<()> {
        let Some(target) = self.online_player_by_name(&name) else {
            return self.deny("A player with this name is not online.").await;
        };
        let Some(name) = self.agent_name(target) else {
            return self.deny("A player with this name is not online.").await;
        };

        self.connection
            .send_message(ServerMessage::PrivateChatOpened { name })
            .await?;
        Ok(())
    }

    pub(super) async fn handle_request_channels(&self) -> Result<()> {
        let channels = self
            .chat
            .get_available_channels()
            .map(|(id, name)| (id, name.to_owned()))
            .collect();
        self.connection
            .send_message(ServerMessage::ChannelList { channels })
            .await?;
        Ok(())
    }

    pub(super) async fn handle_open_channel(&self, channel: ChannelId) -> Result<()> {
        self.chat.join_channel(self.player_key, channel).await;
        Ok(())
    }

    pub(super) async fn handle_close_channel(&self, channel: ChannelId) -> Result<()> {
        self.chat.leave_channel(self.player_key, channel).await;
        Ok(())
    }

    pub(super) async fn agent_said(
        &mut self,
        agent_key: AgentKey,
        position: Position,
        message: String,
    ) -> Result<()> {
        self.send_chat(
            agent_key,
            ChatMessageType::Local,
            ChannelId(0),
            Some(position),
            message,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::connection::ConnectionCommand;
    use crate::actors::session::test_support::seat_player;
    use crate::entities::map::GameMap;
    use crate::entities::position::Position;
    use crate::game::Tick;
    use crate::messages::TextMessageType;

    /// The wire names an author by the character name both sides already know, so
    /// nothing has to be introduced first and no id can go stale.
    #[tokio::test]
    pub(super) async fn a_chat_message_names_its_author() {
        let mut map = GameMap::new();
        let author = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(author, map);

        session
            .receive_private_message(author, "hi".to_owned())
            .await
            .unwrap();

        match connection_rx.try_recv() {
            Ok(ConnectionCommand::SendPlayerMessage(ServerMessage::ChatMessage {
                author, ..
            })) => assert_eq!(author, "Rizael"),
            other => panic!("expected a chat message, got {other:?}"),
        }
    }

    #[tokio::test]
    pub(super) async fn an_author_no_longer_on_the_map_says_nothing() {
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(AgentKey::default(), GameMap::new());

        session
            .receive_private_message(AgentKey::default(), "hi".to_owned())
            .await
            .unwrap();

        assert!(
            connection_rx.try_recv().is_err(),
            "an author the map cannot name must not reach the client unattributed"
        );
    }

    #[tokio::test]
    pub(super) async fn an_over_length_message_is_denied() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(key, map);

        let too_long = "x".repeat(GAME_CONFIG.chat.max_message_length + 1);
        session
            .handle_say(too_long, SayTarget::Local)
            .await
            .unwrap();

        assert!(
            matches!(
                connection_rx.try_recv(),
                Ok(ConnectionCommand::SendPlayerMessage(
                    ServerMessage::TextMessage {
                        message_type: TextMessageType::ActionDenied,
                        ..
                    }
                ))
            ),
            "an over-length message must be refused, not truncated"
        );
    }

    /// The limit counts characters, not bytes, so that the client's input-field cap
    /// measures the same thing. `"é"` is two bytes, so a message of exactly the limit is
    /// over the limit by the old byte rule and within it by the current one — an
    /// all-ASCII test cannot tell the two apart.
    #[tokio::test]
    pub(super) async fn the_length_limit_counts_characters_not_bytes() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, mut world_rx, _tick_tx) =
            SessionActor::for_test(key, map);

        let at_limit = "é".repeat(GAME_CONFIG.chat.max_message_length);
        assert!(
            at_limit.len() > GAME_CONFIG.chat.max_message_length,
            "the fixture must exceed the limit in bytes, or it proves nothing"
        );

        session
            .handle_say(at_limit, SayTarget::Local)
            .await
            .unwrap();

        assert!(
            connection_rx.try_recv().is_err(),
            "a message at the character limit must not be denied"
        );
        assert!(
            matches!(world_rx.try_recv(), Ok((WorldCommand::Say { .. }, _))),
            "a message at the character limit must reach the world"
        );
    }

    /// Enforcement is pinned on the *world* receiver, not the connection. Local speech is
    /// forwarded to `WorldCommand::Say` and never echoed back down the connection, so a
    /// denial arriving on `connection_rx` says the guard fired but not that the message
    /// was actually withheld — only the world receiver shows that. Both are asserted: one
    /// for enforcement, one for the player-facing feedback.
    #[tokio::test]
    pub(super) async fn a_second_message_inside_the_cooldown_is_dropped() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, mut world_rx, tick_tx) =
            SessionActor::for_test(key, map);

        session
            .handle_say("one".to_owned(), SayTarget::Local)
            .await
            .unwrap();
        assert!(
            matches!(world_rx.try_recv(), Ok((WorldCommand::Say { .. }, _))),
            "the first message must reach the world"
        );

        session
            .handle_say("two".to_owned(), SayTarget::Local)
            .await
            .unwrap();
        assert!(
            world_rx.try_recv().is_err(),
            "a second message inside the cooldown must not reach the world"
        );

        assert!(
            matches!(
                connection_rx.try_recv(),
                Ok(ConnectionCommand::SendPlayerMessage(
                    ServerMessage::TextMessage {
                        message_type: TextMessageType::ActionDenied,
                        ..
                    }
                ))
            ),
            "the player must be told why the message did not go through"
        );
        assert!(
            session.next_chat_tick > Tick(0),
            "the cooldown must have been armed"
        );

        // Once the cooldown elapses the same message does get through, so what is being
        // pinned is a delay and not a permanent mute.
        tick_tx
            .send(Tick(GAME_CONFIG.chat.message_cooldown_ticks.0))
            .unwrap();
        session
            .handle_say("three".to_owned(), SayTarget::Local)
            .await
            .unwrap();
        assert!(
            matches!(world_rx.try_recv(), Ok((WorldCommand::Say { .. }, _))),
            "a message sent after the cooldown elapses must reach the world"
        );
    }

    #[tokio::test]
    pub(super) async fn opening_a_pm_chat_with_an_offline_name_is_denied() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(key, map);

        session
            .handle_open_pm_chat("Nobody".to_owned())
            .await
            .unwrap();

        assert!(matches!(
            connection_rx.try_recv(),
            Ok(ConnectionCommand::SendPlayerMessage(
                ServerMessage::TextMessage { .. }
            ))
        ));
    }

    #[tokio::test]
    pub(super) async fn opening_a_pm_chat_confirms_the_target() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(key, map);

        // `a_test_snapshot` names the character "Rizael"; matching is case-insensitive.
        session
            .handle_open_pm_chat("rizael".to_owned())
            .await
            .unwrap();

        match connection_rx.try_recv() {
            Ok(ConnectionCommand::SendPlayerMessage(ServerMessage::PrivateChatOpened { name })) => {
                assert_eq!(
                    name, "Rizael",
                    "the confirmation carries the name as the server spells it, not as it was typed"
                )
            }
            other => panic!("expected a private-chat confirmation, got {other:?}"),
        }
    }

    #[tokio::test]
    pub(super) async fn a_private_message_to_an_offline_name_is_denied() {
        let mut map = GameMap::new();
        let key = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(key, map);

        session
            .handle_say("hi".to_owned(), SayTarget::Player("Nobody".to_owned()))
            .await
            .unwrap();

        assert!(
            matches!(
                connection_rx.try_recv(),
                Ok(ConnectionCommand::SendPlayerMessage(
                    ServerMessage::TextMessage {
                        message_type: TextMessageType::ActionDenied,
                        ..
                    }
                ))
            ),
            "a name that is not online must be refused rather than silently dropped"
        );
    }
}
