use std::collections::HashMap;

use tokio::sync::mpsc;
use tracing::debug;

use crate::{
    actors::message_router::MessageRouterActorHandle,
    config::CONFIG,
    entities::{
        agent::AgentKey,
        chat::{Channel, ChannelId},
    },
    game::config::GAME_CONFIG,
};

#[derive(Debug)]
pub enum ChatCommand {
    SendPrivateMessage {
        sender: AgentKey,
        receiver: AgentKey,
        message: String,
    },
    SendChannelMessage {
        player: AgentKey,
        channel: ChannelId,
        message: String,
    },
    JoinChannel {
        player: AgentKey,
        channel: ChannelId,
    },
    LeaveChannel {
        player: AgentKey,
        channel: ChannelId,
    },
}

#[derive(Clone, Debug)]
pub struct ChatActorHandle {
    tx: mpsc::Sender<ChatCommand>,
    available_channels: Box<[(ChannelId, String)]>,
}

impl ChatActorHandle {
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<ChatCommand>) {
        let (tx, rx) = mpsc::channel(64);
        (
            Self {
                tx,
                available_channels: Box::new([(ChannelId(1), "World Chat".to_owned())]),
            },
            rx,
        )
    }

    pub fn get_available_channels(&self) -> impl Iterator<Item = (ChannelId, &str)> {
        self.available_channels.iter().map(|t| (t.0, t.1.as_str()))
    }

    fn is_server_channel(&self, channel: ChannelId) -> bool {
        self.available_channels.iter().any(|t| t.0 == channel)
    }

    pub async fn join_channel(&self, player: AgentKey, channel: ChannelId) {
        if !self.is_server_channel(channel) {
            return;
        }
        let _ = self
            .tx
            .send(ChatCommand::JoinChannel { player, channel })
            .await;
    }

    pub async fn leave_channel(&self, player: AgentKey, channel: ChannelId) {
        let _ = self
            .tx
            .send(ChatCommand::LeaveChannel { player, channel })
            .await;
    }

    pub async fn message_channel(&self, player: AgentKey, channel: ChannelId, message: String) {
        if !self.is_server_channel(channel) {
            return;
        }
        let _ = self
            .tx
            .send(ChatCommand::SendChannelMessage {
                player,
                channel,
                message,
            })
            .await;
    }

    pub async fn message_player(&self, from: AgentKey, to: AgentKey, message: String) {
        let _ = self
            .tx
            .send(ChatCommand::SendPrivateMessage {
                sender: from,
                receiver: to,
                message,
            })
            .await;
    }
}

pub struct ChatActor {
    rx: mpsc::Receiver<ChatCommand>,
    router: MessageRouterActorHandle,
    channels: HashMap<ChannelId, Channel>,
}

impl ChatActor {
    pub fn start(router: MessageRouterActorHandle) -> ChatActorHandle {
        let (tx, rx) = mpsc::channel(CONFIG.max_buffered_messages);

        let mut channels = HashMap::new();
        for ch in GAME_CONFIG.chat.server_channels.iter() {
            channels.insert(
                ch.id,
                Channel {
                    id: ch.id,
                    name: ch.name.clone(),
                    members: Vec::new(),
                },
            );
        }

        let available_channels: Box<[(ChannelId, String)]> =
            channels.values().map(|c| (c.id, c.name.clone())).collect();

        tokio::spawn(async move {
            let actor = Self {
                rx,
                router,
                channels,
            };
            actor.run().await;
        });

        ChatActorHandle {
            tx,
            available_channels,
        }
    }

    async fn run(mut self) {
        debug!("Chat actor started");

        loop {
            let command = self.rx.recv().await;
            match command {
                Some(command) => self.handle_command(command).await,
                None => break,
            }
        }
    }

    async fn handle_command(&mut self, command: ChatCommand) {
        match command {
            ChatCommand::JoinChannel { player, channel } => {
                self.add_player_to_channel(player, channel)
            }
            ChatCommand::LeaveChannel { player, channel } => {
                self.remove_player_from_channel(player, channel)
            }
            ChatCommand::SendChannelMessage {
                player,
                channel,
                message,
            } => self.send_channel_message(player, channel, message).await,
            ChatCommand::SendPrivateMessage {
                sender,
                receiver,
                message,
            } => self.send_private_message(sender, receiver, message).await,
        }
    }

    fn add_player_to_channel(&mut self, player: AgentKey, channel_id: ChannelId) {
        if let Some(channel) = self.channels.get_mut(&channel_id)
            && !channel.members.contains(&player)
        {
            channel.members.push(player);
        }
    }

    fn remove_player_from_channel(&mut self, player: AgentKey, channel_id: ChannelId) {
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.members.retain(|a| *a != player);
        }
    }

    /// Fans one message out to the channel and prunes whoever the router could not
    /// reach. This lazy prune is the only membership cleanup there is: a player who logs
    /// out of a channel that then goes quiet stays in `members` until the next message.
    /// Harmless — `AgentKey` is a versioned slotmap key, so a recycled key can never
    /// receive a stranger's message.
    async fn send_channel_message(
        &mut self,
        player: AgentKey,
        channel_id: ChannelId,
        message: String,
    ) {
        let Some(channel) = self.channels.get(&channel_id) else {
            return;
        };
        if !channel.members.contains(&player) {
            return;
        }
        let recipients = channel.members.clone();

        let dead = self
            .router
            .deliver_channel_message(player, recipients, channel_id, message)
            .await;

        if dead.is_empty() {
            return;
        }
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.members.retain(|key| !dead.contains(key));
        }
    }

    async fn send_private_message(&self, sender: AgentKey, receiver: AgentKey, message: String) {
        self.router
            .deliver_private_message(sender, receiver, message)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::message_router::MessageRouterCommand;
    use slotmap::SlotMap;
    use std::time::Duration;

    /// `AgentKey::default()` is the slotmap null key, so it is the *only* key reachable
    /// without a slotmap — and two of them are the same key. Mint real, distinct ones.
    fn two_distinct_keys() -> (AgentKey, AgentKey) {
        let mut keys: SlotMap<AgentKey, ()> = SlotMap::with_key();
        (keys.insert(()), keys.insert(()))
    }

    fn a_chat_actor() -> (ChatActor, mpsc::Receiver<MessageRouterCommand>) {
        let (_tx, rx) = mpsc::channel(64);
        let (router, router_rx) = MessageRouterActorHandle::for_test();
        let mut channels = HashMap::new();
        channels.insert(
            ChannelId(1),
            Channel {
                id: ChannelId(1),
                name: "World Chat".to_owned(),
                members: Vec::new(),
            },
        );
        (
            ChatActor {
                rx,
                router,
                channels,
            },
            router_rx,
        )
    }

    #[test]
    fn joining_twice_does_not_duplicate_membership() {
        let (mut actor, _rx) = a_chat_actor();
        let player = AgentKey::default();

        actor.add_player_to_channel(player, ChannelId(1));
        actor.add_player_to_channel(player, ChannelId(1));

        assert_eq!(actor.channels[&ChannelId(1)].members.len(), 1);
    }

    #[test]
    fn joining_an_unknown_channel_is_ignored() {
        let (mut actor, _rx) = a_chat_actor();
        actor.add_player_to_channel(AgentKey::default(), ChannelId(999));
        assert_eq!(actor.channels.len(), 1);
    }

    #[tokio::test]
    async fn a_non_member_cannot_post_to_a_channel() {
        let (mut actor, mut router_rx) = a_chat_actor();

        actor
            .send_channel_message(AgentKey::default(), ChannelId(1), "hello".to_owned())
            .await;

        assert!(
            router_rx.try_recv().is_err(),
            "a non-member's message must never reach the router"
        );
    }

    /// `a_non_member_cannot_post_to_a_channel` posts to an *empty* channel, so it cannot
    /// tell a real membership check from a `members.is_empty()` short-circuit. This one
    /// can: the channel has a member, and a different key does the posting.
    #[tokio::test]
    async fn a_stranger_cannot_post_to_a_channel_that_has_members() {
        let (mut actor, mut router_rx) = a_chat_actor();
        let (member, stranger) = two_distinct_keys();
        actor.add_player_to_channel(member, ChannelId(1));

        // A rejected send returns immediately; a wrongly-accepted one blocks forever on a
        // oneshot nobody answers. The timeout turns that deadlock into a clean assertion.
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            actor.send_channel_message(stranger, ChannelId(1), "hello".to_owned()),
        )
        .await;

        assert!(
            router_rx.try_recv().is_err(),
            "a non-member's message must never reach the router"
        );
    }

    #[tokio::test]
    async fn undeliverable_members_are_pruned() {
        let (mut actor, mut router_rx) = a_chat_actor();
        let player = AgentKey::default();
        actor.add_player_to_channel(player, ChannelId(1));

        // Stand in for the router: answer the oneshot claiming every recipient is dead.
        let router_task = tokio::spawn(async move {
            match router_rx.recv().await.unwrap() {
                MessageRouterCommand::DeliverChannelMessage { recipients, tx, .. } => {
                    tx.send(recipients).unwrap();
                }
                other => panic!("expected DeliverChannelMessage, got {other:?}"),
            }
        });

        actor
            .send_channel_message(player, ChannelId(1), "hello".to_owned())
            .await;
        router_task.await.unwrap();

        assert!(
            actor.channels[&ChannelId(1)].members.is_empty(),
            "a member the router could not reach must be dropped"
        );
    }

    /// `undeliverable_members_are_pruned` kills every member, so it cannot tell a
    /// targeted `retain` from a wholesale `clear`. This one can: only one of the two
    /// members is reported dead. It matters because the prune is the *only* membership
    /// cleanup in the design — an over-reaching one would silently empty every channel
    /// on every message.
    #[tokio::test]
    async fn pruning_removes_only_the_unreachable_members() {
        let (mut actor, mut router_rx) = a_chat_actor();
        let (live, dead) = two_distinct_keys();
        actor.add_player_to_channel(live, ChannelId(1));
        actor.add_player_to_channel(dead, ChannelId(1));

        // Stand in for the router: report exactly one recipient as unreachable.
        let router_task = tokio::spawn(async move {
            match router_rx.recv().await.unwrap() {
                MessageRouterCommand::DeliverChannelMessage { tx, .. } => {
                    tx.send(vec![dead]).unwrap();
                }
                other => panic!("expected DeliverChannelMessage, got {other:?}"),
            }
        });

        actor
            .send_channel_message(live, ChannelId(1), "hello".to_owned())
            .await;
        router_task.await.unwrap();

        assert_eq!(
            actor.channels[&ChannelId(1)].members,
            vec![live],
            "the prune must drop the unreachable member and keep the reachable one"
        );
    }
}
