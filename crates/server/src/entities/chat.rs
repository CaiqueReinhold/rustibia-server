use crate::entities::agent::AgentKey;

/// A chat channel from the server's static list. Global, unlike the ids a session
/// mints, and not on the same schedule as anything a `LocalIdMap` hands out.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, serde::Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct ChannelId(pub u16);

#[derive(Debug)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
    pub members: Vec<AgentKey>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChatMessageType {
    Local,
    Channel,
    Private,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SayTarget {
    Local,
    Channel(ChannelId),
    Player(String),
}
