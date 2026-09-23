use thiserror::Error;
use tokio_util::{
    bytes::{Buf, BufMut, BytesMut},
    codec::{Decoder, Encoder},
};

use crate::{
    constants::view::{MAX_VISIBLE_ITEMS, VIEWPORT_SIZE},
    entities::{
        agent::{AgentId, Facing, OutfitColors, OutfitId, Pool},
        chat::{ChannelId, ChatMessageType, SayTarget},
        effects::{EffectId, MissileId},
        inventory::InventorySlot,
        items::{ClientItemRef, ContainerId, ItemId},
        position::{Direction, Position},
        skills::SkillType,
        spells::{SpellGroup, SpellId, SpellTarget},
    },
};

pub use crate::game::config::Color;

pub type ItemStack = [Option<(ItemId, u8)>; MAX_VISIBLE_ITEMS];

// client
const CLI_PING: u8 = 0;
const CLI_LOGIN: u8 = 1;
const CLI_MOVE_PLAYER: u8 = 2;
const CLI_GET_PLAYER_POS: u8 = 3;
const CLI_MOVE_ITEM: u8 = 4;
const CLI_USE_ITEM: u8 = 5;
const CLI_CLOSE_CONTAINER: u8 = 6;
const CLI_OPEN_PARENT_CONTAINER: u8 = 7;
const CLI_CHANGE_DIRECTION: u8 = 8;
const CLI_LOGOUT: u8 = 9;
const CLI_USE_ITEM_WITH: u8 = 10;
const CLI_LOOK: u8 = 11;
const CLI_SAY: u8 = 12;
const CLI_REQUEST_CHANNELS: u8 = 13;
const CLI_OPEN_CHANNEL: u8 = 14;
const CLI_CLOSE_CHANNEL: u8 = 15;
const CLI_OPEN_PM_CHAT: u8 = 16;
const CLI_SET_TARGET: u8 = 17;
const CLI_CAST_SPELL: u8 = 18;

#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    Ping,
    Login {
        auth_token: String,
    },
    MovePlayer {
        direction: Direction,
    },
    GetPlayerPosition,
    MoveItem {
        item: ClientItemRef,
        amount: u8,
        to: Position,
    },
    UseItem {
        item: ClientItemRef,
    },
    CloseContainer {
        container_id: ContainerId,
    },
    OpenParentContainer {
        container_id: ContainerId,
    },
    ChangeDirection {
        direction: Facing,
    },
    Logout,
    UseItemWith {
        source: ClientItemRef,
        target: ClientItemRef,
        target_agent: Option<AgentId>,
    },
    Look {
        position: Position,
    },
    Say {
        message: String,
        target: SayTarget,
    },
    RequestChannels,
    OpenChannel {
        channel: ChannelId,
    },
    CloseChannel {
        channel: ChannelId,
    },
    OpenPmChat {
        name: String,
    },
    SetTarget {
        agent_id: Option<AgentId>,
        seq: u32,
    },
    CastSpell {
        spell_id: SpellId,
        target: SpellTarget,
        param: Option<String>,
    },
}

// server
const SRV_PONG: u8 = 0;
const SRV_LOGIN_ERROR: u8 = 1;
const SRV_DESCRIBE_MAP: u8 = 2;
const SRV_TILE_UPDATED: u8 = 3;
const SRV_PLAYER_WALK_ACK: u8 = 4;
const SRV_PLAYER_POS: u8 = 5;
const SRV_DESCRIBE_PLAYER: u8 = 6;
const SRV_TEXT_MESSAGE: u8 = 7;
const SRV_OPEN_CONTAINER: u8 = 8;
const SRV_UPDATE_CONTAINER: u8 = 9;
const SRV_CONTAINER_CLOSED: u8 = 10;
const SRV_PLAYER_WALK_DENIED: u8 = 11;
const SRV_INVETORY_SLOT_UPDATED: u8 = 12;
const SRV_PLAYER_CAPACITY_UPDATED: u8 = 13;
const SRV_AGENT_DIRECTION_CHANGED: u8 = 14;
const SRV_REMOVE_AGENT: u8 = 15;
const SRV_MOVE_AGENT: u8 = 16;
const SRV_SPAWN_AGENT: u8 = 17;
const SRV_TELEPORT_AGENT: u8 = 18;
const SRV_CHAT_MESSAGE: u8 = 19;
const SRV_CHANNEL_LIST: u8 = 20;
const SRV_PRIVATE_CHAT_OPENED: u8 = 21;
const SRV_FLOATING_TEXT: u8 = 22;
const SRV_TARGET_LOST: u8 = 23;
const SRV_AGENT_LIFE_UPDATED: u8 = 24;
const SRV_SHOW_EFFECT: u8 = 25;
const SRV_LAUNCH_MISSILE: u8 = 26;
const SRV_AGENT_MANA_UPDATED: u8 = 27;
const SRV_PLAYER_SKILLS: u8 = 28;
const SRV_SKILL_UPDATED: u8 = 29;
const SRV_EXPERIENCE_UPDATED: u8 = 30;
const SRV_SPELL_CAST: u8 = 31;
const SRV_SPELL_LIST: u8 = 32;
const SRV_AGENT_SPEED_UPDATED: u8 = 33;
const SRV_PLAYER_STATUS: u8 = 34;
const SRV_DAMAGED_BY: u8 = 35;

#[derive(Clone, Debug, PartialEq)]
pub enum TextMessageType {
    ActionDenied,
    Look,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkillProgress {
    pub level: u16,
    pub percent_bp: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FloatingTextType {
    HitPoints,
    CreatureSay,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpellListEntry {
    pub id: SpellId,
    pub name: String,
    pub words: String,
    pub level: u16,
    pub icon: u16,
    pub aimable: bool,
    pub group: SpellGroup,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    Pong,
    LoginError,
    DescribePlayer {
        agent_id: AgentId,
        position: Position,
        facing: Facing,
        name: String,
        level: u16,
        life: Pool,
        mana: Pool,
        outfit: (OutfitId, OutfitColors),
        speed: u16,
        capacity: u32,
        inventory_head: Option<ItemId>,
        inventory_amulet: Option<ItemId>,
        inventory_backpack: Option<ItemId>,
        inventory_chest: Option<ItemId>,
        inventory_right_hand: Option<ItemId>,
        inventory_left_hand: Option<ItemId>,
        inventory_legs: Option<ItemId>,
        inventory_feet: Option<ItemId>,
        inventory_ring: Option<ItemId>,
        inventory_trinket: Option<ItemId>,
    },
    DescribeMap {
        tiles: Box<[ItemStack; VIEWPORT_SIZE]>,
        center: Position,
        floor: u8,
    },
    TileUpdated {
        position: Position,
        items: Box<ItemStack>,
    },
    PlayerWalkAck {
        position: Position,
        tiles: Vec<(u8, Box<[ItemStack]>)>, // (floor, tiles)
    },
    PlayerPosition {
        position: Position,
    },
    TextMessage {
        text: String,
        message_type: TextMessageType,
    },
    OpenContainer {
        container_id: ContainerId,
        capacity: u8,
        has_parent: bool,
        title: String,
        items: Box<[Option<(ItemId, u8)>]>,
    },
    UpdateContainer {
        container_id: ContainerId,
        items: Box<[Option<(ItemId, u8)>]>,
    },
    ContainerClosed {
        container_id: ContainerId,
    },
    PlayerWalkDenied,
    IventorySlotUpdated {
        slot: InventorySlot,
        item_id: Option<ItemId>,
    },
    PlayerCapacityUpdated {
        cap: u32,
    },
    AgentChangedDirection {
        agent_id: AgentId,
        facing: Facing,
    },
    RemoveAgent {
        agent_id: AgentId,
    },
    MoveAgent {
        agent_id: AgentId,
        direction: Direction,
        from: Position,
    },
    SpawnAgent {
        agent_id: AgentId,
        outfit: (OutfitId, OutfitColors),
        position: Position,
        facing: Facing,
        name: String,
        life: u32,
        speed: u16,
    },
    TeleportAgent {
        agent_id: AgentId,
        position: Position,
    },
    ChatMessage {
        author: String,
        message_type: ChatMessageType,
        channel: ChannelId,
        position: Option<Position>,
        message: String,
    },
    ChannelList {
        channels: Vec<(ChannelId, String)>,
    },
    PrivateChatOpened {
        name: String,
    },
    FloatingText {
        text: String,
        position: Position,
        text_type: FloatingTextType,
        color: Option<Color>,
    },
    TargetLost {
        seq: u32,
    },
    DamagedBy {
        agent_id: AgentId,
    },
    ShowEffect {
        effect_id: EffectId,
        position: Position,
        delta: Vec<(i8, i8)>,
    },
    AgentLifeChanged {
        agent_id: AgentId,
        current: u32,
        max: u32,
    },
    LaunchMissile {
        from: Position,
        to: Position,
        missile_id: MissileId,
    },
    AgentManaUpdated {
        agent_id: AgentId,
        current: u32,
        max: u32,
    },
    PlayerSkills {
        experience: u64,
        skills: Vec<(SkillType, SkillProgress)>,
    },
    SkillUpdated {
        skill: SkillType,
        progress: SkillProgress,
    },
    ExperienceUpdated {
        experience: u64,
    },
    SpellCast {
        spell: SpellId,
        spell_cooldown_ms: u32,
        group_cooldown_ms: u32,
    },
    SpellList {
        spells: Vec<SpellListEntry>,
    },
    AgentSpeedUpdated {
        agent_id: AgentId,
        speed: u16,
    },
    PlayerStatus {
        status: u32,
    },
}

#[derive(Error, Debug)]
pub enum MessageDecodeError {
    #[error("Read error")]
    ReadError(#[from] std::io::Error),
    #[error("Wrong sequence")]
    WrongSequence,
}

pub struct GameMessageCodec {}

impl Decoder for GameMessageCodec {
    type Item = ClientMessage;
    type Error = MessageDecodeError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if buf.len() < 2 {
            return Ok(None);
        }

        let payload_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;

        if buf.len() < 2 + payload_len {
            return Ok(None);
        }

        if payload_len == 0 {
            return Err(MessageDecodeError::WrongSequence);
        }

        buf.advance(2);

        match buf.get_u8() {
            CLI_PING => Ok(Some(ClientMessage::Ping)),
            CLI_LOGIN => {
                // Cannot underflow: `payload_len` is at least 1, checked above.
                let token_len = payload_len - 1; // subtract the message type byte
                let auth_token = String::from_utf8(buf.split_to(token_len).to_vec())
                    .map_err(|_| MessageDecodeError::WrongSequence)?;
                Ok(Some(ClientMessage::Login { auth_token }))
            }
            CLI_MOVE_PLAYER => {
                let direction = decode_direction(buf.get_u8())?;
                Ok(Some(ClientMessage::MovePlayer { direction }))
            }
            CLI_GET_PLAYER_POS => Ok(Some(ClientMessage::GetPlayerPosition)),
            CLI_MOVE_ITEM => {
                let from = decode_position(buf);
                let item_id = ItemId(buf.get_u16_le());
                let amount = buf.get_u8();
                let stack_index = buf.get_u8();
                let to = decode_position(buf);
                Ok(Some(ClientMessage::MoveItem {
                    item: ClientItemRef {
                        position: from,
                        item_id,
                        stack_index,
                    },
                    amount,
                    to,
                }))
            }
            CLI_USE_ITEM => Ok(Some(ClientMessage::UseItem {
                item: ClientItemRef {
                    position: decode_position(buf),
                    item_id: ItemId(buf.get_u16_le()),
                    stack_index: buf.get_u8(),
                },
            })),
            CLI_CLOSE_CONTAINER => Ok(Some(ClientMessage::CloseContainer {
                container_id: ContainerId(buf.get_u16_le()),
            })),
            CLI_OPEN_PARENT_CONTAINER => Ok(Some(ClientMessage::OpenParentContainer {
                container_id: ContainerId(buf.get_u16_le()),
            })),
            CLI_CHANGE_DIRECTION => Ok(Some(ClientMessage::ChangeDirection {
                direction: decode_facing(buf.get_u8())?,
            })),
            CLI_LOGOUT => Ok(Some(ClientMessage::Logout)),
            CLI_USE_ITEM_WITH => Ok(Some(ClientMessage::UseItemWith {
                source: ClientItemRef {
                    position: decode_position(buf),
                    item_id: ItemId(buf.get_u16_le()),
                    stack_index: buf.get_u8(),
                },
                target: ClientItemRef {
                    position: decode_position(buf),
                    item_id: ItemId(buf.get_u16_le()),
                    stack_index: buf.get_u8(),
                },
                target_agent: decode_optional_agent(buf.get_u16_le()),
            })),
            CLI_LOOK => Ok(Some(ClientMessage::Look {
                position: decode_position(buf),
            })),
            CLI_SAY => {
                if payload_len < 2 {
                    return Err(MessageDecodeError::WrongSequence);
                }
                let mut remaining = payload_len - 2;
                let target = match decode_chat_message_type(buf.get_u8())? {
                    ChatMessageType::Local => SayTarget::Local,
                    ChatMessageType::Channel => {
                        if remaining < 2 {
                            return Err(MessageDecodeError::WrongSequence);
                        }
                        remaining -= 2;
                        SayTarget::Channel(ChannelId(buf.get_u16_le()))
                    }
                    ChatMessageType::Private => {
                        if remaining < 2 {
                            return Err(MessageDecodeError::WrongSequence);
                        }
                        let name_len = buf.get_u16_le() as usize;
                        remaining -= 2;
                        if name_len > remaining {
                            return Err(MessageDecodeError::WrongSequence);
                        }
                        remaining -= name_len;
                        let name = String::from_utf8(buf.split_to(name_len).to_vec())
                            .map_err(|_| MessageDecodeError::WrongSequence)?;
                        SayTarget::Player(name)
                    }
                };
                let message = String::from_utf8(buf.split_to(remaining).to_vec())
                    .map_err(|_| MessageDecodeError::WrongSequence)?;
                Ok(Some(ClientMessage::Say { message, target }))
            }
            CLI_REQUEST_CHANNELS => Ok(Some(ClientMessage::RequestChannels)),
            CLI_OPEN_CHANNEL => Ok(Some(ClientMessage::OpenChannel {
                channel: ChannelId(buf.get_u16_le()),
            })),
            CLI_CLOSE_CHANNEL => Ok(Some(ClientMessage::CloseChannel {
                channel: ChannelId(buf.get_u16_le()),
            })),
            CLI_OPEN_PM_CHAT => {
                // Cannot underflow: `payload_len` is at least 1, checked above.
                let name = String::from_utf8(buf.split_to(payload_len - 1).to_vec())
                    .map_err(|_| MessageDecodeError::WrongSequence)?;
                Ok(Some(ClientMessage::OpenPmChat { name }))
            }
            CLI_SET_TARGET => Ok(Some(ClientMessage::SetTarget {
                agent_id: decode_optional_agent(buf.get_u16_le()),
                seq: buf.get_u32_le(),
            })),
            CLI_CAST_SPELL => Ok(Some(ClientMessage::CastSpell {
                spell_id: SpellId(buf.get_u16_le()),
                target: decode_spell_target(buf)?,
                param: decode_param(buf)?,
            })),
            _ => Err(MessageDecodeError::WrongSequence),
        }
    }
}

fn decode_position(buf: &mut BytesMut) -> Position {
    Position {
        x: buf.get_u16_le(),
        y: buf.get_u16_le(),
        z: buf.get_u8(),
    }
}

fn decode_direction(b: u8) -> Result<Direction, MessageDecodeError> {
    match b {
        0x00 => Ok(Direction::North),
        0x01 => Ok(Direction::East),
        0x02 => Ok(Direction::West),
        0x03 => Ok(Direction::South),
        0x04 => Ok(Direction::NorthEast),
        0x05 => Ok(Direction::NorthWest),
        0x06 => Ok(Direction::SouthEast),
        0x07 => Ok(Direction::SouthWest),
        _ => Err(MessageDecodeError::WrongSequence),
    }
}

fn decode_facing(b: u8) -> Result<Facing, MessageDecodeError> {
    match b {
        1 => Ok(Facing::North),
        2 => Ok(Facing::East),
        3 => Ok(Facing::South),
        4 => Ok(Facing::West),
        _ => Err(MessageDecodeError::WrongSequence),
    }
}

fn decode_chat_message_type(b: u8) -> Result<ChatMessageType, MessageDecodeError> {
    match b {
        0x01 => Ok(ChatMessageType::Local),
        0x02 => Ok(ChatMessageType::Private),
        0x03 => Ok(ChatMessageType::Channel),
        _ => Err(MessageDecodeError::WrongSequence),
    }
}

fn decode_spell_target(buf: &mut BytesMut) -> Result<SpellTarget, MessageDecodeError> {
    match buf.get_u8() {
        0x00 => Ok(SpellTarget::None),
        0x01 => Ok(SpellTarget::Agent(AgentId(buf.get_u16_le()))),
        0x02 => Ok(SpellTarget::Position(decode_position(buf))),
        _ => Err(MessageDecodeError::WrongSequence),
    }
}

/// Length-prefixed and empty rather than absent for a cast that names nothing, so the field
/// is the same width to skip whatever the spell is.
fn decode_param(buf: &mut BytesMut) -> Result<Option<String>, MessageDecodeError> {
    let len = buf.get_u16_le() as usize;
    if len == 0 {
        return Ok(None);
    }
    String::from_utf8(buf.split_to(len).to_vec())
        .map(Some)
        .map_err(|_| MessageDecodeError::WrongSequence)
}

#[derive(Error, Debug)]
pub enum MessageEncodeError {
    #[error("Read error")]
    ReadError(#[from] std::io::Error),
}

impl Encoder<ServerMessage> for GameMessageCodec {
    type Error = MessageEncodeError;

    fn encode(&mut self, item: ServerMessage, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let len_offset = dst.len();
        dst.put_u16_le(0); // placeholder for payload length

        match item {
            ServerMessage::Pong => dst.put_u8(SRV_PONG),
            ServerMessage::LoginError => dst.put_u8(SRV_LOGIN_ERROR),
            ServerMessage::DescribePlayer {
                agent_id,
                position,
                facing,
                name,
                level,
                life,
                mana,
                outfit,
                speed,
                capacity,
                inventory_head,
                inventory_amulet,
                inventory_backpack,
                inventory_chest,
                inventory_right_hand,
                inventory_left_hand,
                inventory_legs,
                inventory_feet,
                inventory_ring,
                inventory_trinket,
            } => {
                dst.put_u8(SRV_DESCRIBE_PLAYER);
                dst.put_u16_le(agent_id.0);
                encode_position(position, dst);
                encode_facing(facing, dst);
                encode_string(&name, dst);
                dst.put_u16_le(level);
                encode_pool(&life, dst);
                encode_pool(&mana, dst);
                encode_outfit(outfit, dst);
                dst.put_u16_le(speed);
                dst.put_u32_le(capacity);
                encode_optional_item(inventory_head, dst);
                encode_optional_item(inventory_amulet, dst);
                encode_optional_item(inventory_backpack, dst);
                encode_optional_item(inventory_chest, dst);
                encode_optional_item(inventory_right_hand, dst);
                encode_optional_item(inventory_left_hand, dst);
                encode_optional_item(inventory_legs, dst);
                encode_optional_item(inventory_feet, dst);
                encode_optional_item(inventory_ring, dst);
                encode_optional_item(inventory_trinket, dst);
            }
            ServerMessage::DescribeMap {
                tiles,
                center,
                floor,
            } => {
                dst.put_u8(SRV_DESCRIBE_MAP);
                encode_position(center, dst);
                dst.put_u8(floor);
                for tile in tiles.iter() {
                    encode_tile(tile.as_ref(), dst);
                }
            }
            ServerMessage::TileUpdated { position, items } => {
                dst.put_u8(SRV_TILE_UPDATED);
                encode_position(position, dst);
                encode_tile(items.as_ref(), dst);
            }
            ServerMessage::PlayerWalkAck { position, tiles } => {
                dst.put_u8(SRV_PLAYER_WALK_ACK);
                encode_position(position, dst);
                for (floor, tiles) in tiles.iter() {
                    dst.put_u8(*floor);
                    dst.put_u8(tiles.len() as u8);
                    for tile in tiles.iter() {
                        encode_tile(tile, dst);
                    }
                }
                dst.put_u8(0xFF);
            }
            ServerMessage::PlayerPosition { position } => {
                dst.put_u8(SRV_PLAYER_POS);
                encode_position(position, dst);
            }
            ServerMessage::TextMessage { text, message_type } => {
                dst.put_u8(SRV_TEXT_MESSAGE);
                encode_string(&text, dst);
                dst.put_u8(encode_text_message_type(message_type));
            }
            ServerMessage::OpenContainer {
                container_id,
                capacity,
                has_parent,
                title,
                items,
            } => {
                dst.put_u8(SRV_OPEN_CONTAINER);
                dst.put_u16_le(container_id.0);
                dst.put_u8(capacity);
                dst.put_u8(if has_parent { 1 } else { 0 });
                encode_short_string(&title, dst);
                encode_tile(&items, dst);
            }
            ServerMessage::UpdateContainer {
                container_id,
                items,
            } => {
                dst.put_u8(SRV_UPDATE_CONTAINER);
                dst.put_u16_le(container_id.0);
                encode_tile(&items, dst);
            }
            ServerMessage::ContainerClosed { container_id } => {
                dst.put_u8(SRV_CONTAINER_CLOSED);
                dst.put_u16_le(container_id.0);
            }
            ServerMessage::PlayerWalkDenied => dst.put_u8(SRV_PLAYER_WALK_DENIED),
            ServerMessage::IventorySlotUpdated { slot, item_id } => {
                dst.put_u8(SRV_INVETORY_SLOT_UPDATED);
                dst.put_u8(slot.as_id());
                encode_optional_item(item_id, dst);
            }
            ServerMessage::PlayerCapacityUpdated { cap } => {
                dst.put_u8(SRV_PLAYER_CAPACITY_UPDATED);
                dst.put_u32_le(cap);
            }
            ServerMessage::AgentChangedDirection { agent_id, facing } => {
                dst.put_u8(SRV_AGENT_DIRECTION_CHANGED);
                dst.put_u16_le(agent_id.0);
                encode_facing(facing, dst);
            }
            ServerMessage::RemoveAgent { agent_id } => {
                dst.put_u8(SRV_REMOVE_AGENT);
                dst.put_u16_le(agent_id.0);
            }
            ServerMessage::MoveAgent {
                agent_id,
                direction,
                from,
            } => {
                dst.put_u8(SRV_MOVE_AGENT);
                dst.put_u16_le(agent_id.0);
                encode_direction(&direction, dst);
                encode_position(from, dst);
            }
            ServerMessage::SpawnAgent {
                agent_id,
                outfit,
                position,
                facing,
                name,
                life,
                speed,
            } => {
                dst.put_u8(SRV_SPAWN_AGENT);
                dst.put_u16_le(agent_id.0);
                encode_position(position, dst);
                encode_facing(facing, dst);
                encode_string(&name, dst);
                dst.put_u32_le(life);
                encode_outfit(outfit, dst);
                dst.put_u16_le(speed);
            }
            ServerMessage::TeleportAgent { agent_id, position } => {
                dst.put_u8(SRV_TELEPORT_AGENT);
                dst.put_u16_le(agent_id.0);
                encode_position(position, dst);
            }
            ServerMessage::ChatMessage {
                author,
                message_type,
                channel,
                position,
                message,
            } => {
                dst.put_u8(SRV_CHAT_MESSAGE);
                encode_string(&author, dst);
                dst.put_u8(encode_chat_message_type(message_type));
                dst.put_u16_le(channel.0);
                match position {
                    Some(position) => {
                        dst.put_u8(0x01);
                        encode_position(position, dst);
                    }
                    None => dst.put_u8(0x00),
                }
                encode_string(&message, dst);
            }
            ServerMessage::ChannelList { channels } => {
                dst.put_u8(SRV_CHANNEL_LIST);
                dst.put_u16_le(channels.len() as u16);
                for (id, name) in channels.iter() {
                    dst.put_u16_le(id.0);
                    encode_string(name, dst);
                }
            }
            ServerMessage::PrivateChatOpened { name } => {
                dst.put_u8(SRV_PRIVATE_CHAT_OPENED);
                encode_string(&name, dst);
            }
            ServerMessage::FloatingText {
                text,
                position,
                text_type,
                color,
            } => {
                dst.put_u8(SRV_FLOATING_TEXT);
                encode_string(&text, dst);
                encode_position(position, dst);
                dst.put_u8(encode_floating_text_type(text_type));
                match color {
                    Some(Color(r, g, b)) => {
                        dst.put_u8(0x01);
                        dst.put_u8(r);
                        dst.put_u8(g);
                        dst.put_u8(b);
                    }
                    None => dst.put_u8(0x00),
                }
            }
            ServerMessage::TargetLost { seq } => {
                dst.put_u8(SRV_TARGET_LOST);
                dst.put_u32_le(seq);
            }
            ServerMessage::DamagedBy { agent_id } => {
                dst.put_u8(SRV_DAMAGED_BY);
                dst.put_u16_le(agent_id.0);
            }
            ServerMessage::AgentLifeChanged {
                agent_id,
                current,
                max,
            } => {
                dst.put_u8(SRV_AGENT_LIFE_UPDATED);
                dst.put_u16_le(agent_id.0);
                dst.put_u32_le(current);
                dst.put_u32_le(max);
            }
            ServerMessage::ShowEffect {
                effect_id,
                position,
                delta,
            } => {
                dst.put_u8(SRV_SHOW_EFFECT);
                dst.put_u16_le(effect_id.0);
                encode_position(position, dst);
                for (dx, dy) in delta {
                    dst.put_i8(dx);
                    dst.put_i8(dy);
                }
            }
            ServerMessage::LaunchMissile {
                from,
                to,
                missile_id,
            } => {
                dst.put_u8(SRV_LAUNCH_MISSILE);
                encode_position(from, dst);
                encode_position(to, dst);
                dst.put_u16_le(missile_id.0);
            }
            ServerMessage::AgentManaUpdated {
                agent_id,
                current,
                max,
            } => {
                dst.put_u8(SRV_AGENT_MANA_UPDATED);
                dst.put_u16_le(agent_id.0);
                dst.put_u32_le(current);
                dst.put_u32_le(max);
            }
            ServerMessage::PlayerSkills { experience, skills } => {
                dst.put_u8(SRV_PLAYER_SKILLS);
                dst.put_u64_le(experience);
                dst.put_u8(skills.len() as u8);
                for (skill, progress) in skills {
                    dst.put_u8(skill.as_id());
                    dst.put_u16_le(progress.level);
                    dst.put_u16_le(progress.percent_bp);
                }
            }
            ServerMessage::SkillUpdated { skill, progress } => {
                dst.put_u8(SRV_SKILL_UPDATED);
                dst.put_u8(skill.as_id());
                dst.put_u16_le(progress.level);
                dst.put_u16_le(progress.percent_bp);
            }
            ServerMessage::ExperienceUpdated { experience } => {
                dst.put_u8(SRV_EXPERIENCE_UPDATED);
                dst.put_u64_le(experience);
            }
            ServerMessage::SpellCast {
                spell,
                spell_cooldown_ms,
                group_cooldown_ms,
            } => {
                dst.put_u8(SRV_SPELL_CAST);
                dst.put_u16_le(spell.0);
                dst.put_u32_le(spell_cooldown_ms);
                dst.put_u32_le(group_cooldown_ms);
            }
            ServerMessage::SpellList { spells } => {
                dst.put_u8(SRV_SPELL_LIST);
                dst.put_u16_le(spells.len() as u16);
                for spell in spells {
                    dst.put_u16_le(spell.id.0);
                    encode_string(&spell.name, dst);
                    encode_string(&spell.words, dst);
                    dst.put_u16_le(spell.level);
                    dst.put_u16_le(spell.icon);
                    dst.put_u8(u8::from(spell.aimable));
                    dst.put_u8(spell.group.as_id());
                }
            }
            ServerMessage::AgentSpeedUpdated { agent_id, speed } => {
                dst.put_u8(SRV_AGENT_SPEED_UPDATED);
                dst.put_u16_le(agent_id.0);
                dst.put_u16_le(speed);
            }
            ServerMessage::PlayerStatus { status } => {
                dst.put_u8(SRV_PLAYER_STATUS);
                dst.put_u32_le(status);
            }
        }

        let payload_len = (dst.len() - len_offset - 2) as u16;
        dst[len_offset..len_offset + 2].copy_from_slice(&payload_len.to_le_bytes());

        Ok(())
    }
}

fn encode_position(pos: Position, dst: &mut BytesMut) {
    dst.put_u16_le(pos.x);
    dst.put_u16_le(pos.y);
    dst.put_u8(pos.z);
}

fn encode_facing(facing: Facing, dst: &mut BytesMut) {
    match facing {
        Facing::North => dst.put_u8(1),
        Facing::East => dst.put_u8(2),
        Facing::South => dst.put_u8(3),
        Facing::West => dst.put_u8(4),
    }
}

fn encode_direction(d: &Direction, dst: &mut BytesMut) {
    let value = match d {
        Direction::North => 0x00,
        Direction::East => 0x01,
        Direction::West => 0x02,
        Direction::South => 0x03,
        Direction::NorthEast => 0x04,
        Direction::NorthWest => 0x05,
        Direction::SouthEast => 0x06,
        Direction::SouthWest => 0x07,
    };
    dst.put_u8(value);
}

fn encode_string(s: &str, dst: &mut BytesMut) {
    let bytes = s.as_bytes();
    dst.put_u16_le(bytes.len() as u16);
    dst.put_slice(bytes);
}

/// A container title is the one string the wire length-prefixes with a `u8`. The client reads
/// the two widths differently, so this is the message's shape rather than a choice — merging it
/// into `encode_string` desynchronises the reader.
fn encode_short_string(s: &str, dst: &mut BytesMut) {
    let bytes = s.as_bytes();
    dst.put_u8(bytes.len() as u8);
    dst.put_slice(bytes);
}

fn encode_outfit(outfit: (OutfitId, OutfitColors), dst: &mut BytesMut) {
    dst.put_u16_le(outfit.0.0);
    dst.put_u8(outfit.1.head);
    dst.put_u8(outfit.1.body);
    dst.put_u8(outfit.1.legs);
    dst.put_u8(outfit.1.feet);
}

fn encode_pool(pool: &Pool, dst: &mut BytesMut) {
    dst.put_u32_le(pool.current);
    dst.put_u32_le(pool.maximum);
}

fn encode_tile(items: &[Option<(ItemId, u8)>], dst: &mut BytesMut) {
    for item in items {
        match item {
            Some((id, amount)) => {
                dst.put_u16_le(id.0);
                dst.put_u8(*amount);
            }
            None => break,
        }
    }
    dst.put_u16_le(0xFFFF);
}

fn encode_text_message_type(text_type: TextMessageType) -> u8 {
    match text_type {
        TextMessageType::ActionDenied => 0x01,
        TextMessageType::Look => 0x02,
    }
}

fn encode_optional_item(item_id: Option<ItemId>, dst: &mut BytesMut) {
    if let Some(item_id) = item_id {
        dst.put_u16_le(item_id.0);
    } else {
        dst.put_u16_le(0xFFFF);
    }
}

fn decode_optional_agent(raw: u16) -> Option<AgentId> {
    if raw == 0xFFFF {
        None
    } else {
        Some(AgentId(raw))
    }
}

fn encode_chat_message_type(message_type: ChatMessageType) -> u8 {
    match message_type {
        ChatMessageType::Local => 0x01,
        ChatMessageType::Private => 0x02,
        ChatMessageType::Channel => 0x03,
    }
}

fn encode_floating_text_type(text_type: FloatingTextType) -> u8 {
    match text_type {
        FloatingTextType::HitPoints => 0x01,
        FloatingTextType::CreatureSay => 0x02,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::bytes::BytesMut;
    use tokio_util::codec::Decoder;
    use tokio_util::codec::Encoder;

    /// The client repeats this enum and decodes these bytes; nothing links the two
    /// but this number.
    #[test]
    fn creature_say_is_two_on_the_wire() {
        assert_eq!(
            encode_floating_text_type(FloatingTextType::CreatureSay),
            0x02
        );
    }

    /// The client repeats these bit positions; nothing links the two but this frame.
    #[test]
    fn a_player_status_is_a_little_endian_bitset() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        codec
            .encode(ServerMessage::PlayerStatus { status: 0x0A }, &mut buf)
            .unwrap();

        assert_eq!(&buf[..], &[5u8, 0, SRV_PLAYER_STATUS, 0x0A, 0, 0, 0]);
    }

    #[test]
    fn decode_logout_message() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        // 2-byte LE length prefix (1 byte payload) + 1 byte type
        buf.extend_from_slice(&[1u8, 0u8, CLI_LOGOUT]);
        let msg = codec.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(msg, ClientMessage::Logout));
        assert!(buf.is_empty());
    }

    /// The frame the client now sends: length, type byte, then the token and nothing
    /// else. The character is no longer the client's to name.
    #[test]
    fn decode_login_message() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let token = b"a-token";
        let payload_len = (1 + token.len()) as u16;

        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&[CLI_LOGIN]);
        buf.extend_from_slice(token);

        let msg = codec.decode(&mut buf).unwrap().unwrap();

        match msg {
            ClientMessage::Login { auth_token } => assert_eq!(auth_token, "a-token"),
            other => panic!("expected Login, got {other:?}"),
        }
        assert!(buf.is_empty(), "the frame must be fully consumed");
    }

    /// A frame in the old layout is not rejected structurally — `token_len` comes from
    /// the frame length, so the 4-byte id is simply eaten as part of the token. Whether
    /// that yields a junk token (refused later, at redemption) or a decode error
    /// depends on the id's bytes. Both are fine. What must not happen is a panic in
    /// `from_utf8` or on a short buffer.
    #[test]
    fn an_old_format_login_frame_fails_without_panicking() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let token = b"a-token";
        let payload_len = (1 + 4 + token.len()) as u16;

        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&[CLI_LOGIN]);
        buf.extend_from_slice(&7u32.to_le_bytes());
        buf.extend_from_slice(token);

        match codec.decode(&mut buf) {
            Ok(Some(ClientMessage::Login { auth_token })) => assert_ne!(
                auth_token, "a-token",
                "the id bytes must land inside the token, not be silently skipped"
            ),
            Err(MessageDecodeError::WrongSequence) => {}
            other => panic!("expected a junk token or WrongSequence, got {other:?}"),
        }
    }

    #[test]
    fn use_item_with_carries_an_optional_target_agent() {
        let mut codec = GameMessageCodec {};
        let mut payload = BytesMut::new();
        payload.put_u8(CLI_USE_ITEM_WITH);
        payload.put_u16_le(10);
        payload.put_u16_le(11);
        payload.put_u8(7);
        payload.put_u16_le(1234);
        payload.put_u8(0);
        payload.put_u16_le(12);
        payload.put_u16_le(13);
        payload.put_u8(7);
        payload.put_u16_le(5678);
        payload.put_u8(1);
        payload.put_u16_le(42);

        let mut buf = BytesMut::new();
        buf.put_u16_le(payload.len() as u16);
        buf.extend_from_slice(&payload);

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(
            decoded,
            ClientMessage::UseItemWith {
                target_agent: Some(AgentId(42)),
                ..
            }
        ));
    }

    #[test]
    fn the_optional_target_agent_sentinel_decodes_as_none() {
        let mut codec = GameMessageCodec {};
        let mut payload = BytesMut::new();
        payload.put_u8(CLI_USE_ITEM_WITH);
        payload.put_u16_le(10);
        payload.put_u16_le(11);
        payload.put_u8(7);
        payload.put_u16_le(1234);
        payload.put_u8(0);
        payload.put_u16_le(12);
        payload.put_u16_le(13);
        payload.put_u8(7);
        payload.put_u16_le(5678);
        payload.put_u8(1);
        payload.put_u16_le(0xFFFF);

        let mut buf = BytesMut::new();
        buf.put_u16_le(payload.len() as u16);
        buf.extend_from_slice(&payload);

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(
            decoded,
            ClientMessage::UseItemWith {
                target_agent: None,
                ..
            }
        ));
    }

    /// A two-byte frame declaring a zero-length payload. Before the length guard this
    /// panicked in `get_u8` on the empty buffer `advance(2)` left behind — reachable by
    /// anyone who can open a socket, and for every message type, not just login.
    #[test]
    fn a_zero_length_payload_is_rejected_and_does_not_panic() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&0u16.to_le_bytes());

        assert!(
            matches!(
                codec.decode(&mut buf),
                Err(MessageDecodeError::WrongSequence)
            ),
            "a zero-length payload must be a decode error, never a panic"
        );
    }

    /// The same malformed length, but with a type byte present so the old code reached
    /// `payload_len - 1`. That subtraction underflowed: a panic in debug, and in release
    /// a wrap to `usize::MAX` that panicked inside `split_to`'s bounds check instead.
    #[test]
    fn a_login_frame_claiming_zero_length_does_not_underflow() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&[CLI_LOGIN]);

        assert!(
            matches!(
                codec.decode(&mut buf),
                Err(MessageDecodeError::WrongSequence)
            ),
            "a zero-length login payload must be a decode error, never a panic"
        );
    }

    #[test]
    fn encode_private_chat_opened() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        codec
            .encode(
                ServerMessage::PrivateChatOpened {
                    name: "Rizael".to_owned(),
                },
                &mut buf,
            )
            .unwrap();

        let payload_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        assert_eq!(
            payload_len,
            buf.len() - 2,
            "length prefix must cover the payload"
        );
        assert_eq!(buf[2], SRV_PRIVATE_CHAT_OPENED);
        assert_eq!(u16::from_le_bytes([buf[3], buf[4]]), 6);
        assert_eq!(&buf[5..], b"Rizael");
    }

    /// The position is an `Option` behind a flag byte, exactly like the colour on
    /// `FloatingText`, so the two forms differ in length and a decoder that always
    /// reads five position bytes corrupts every non-local line. The client decodes
    /// this same literal frame in `decodes_a_local_chat_message`.
    #[test]
    fn encode_chat_message_carries_the_speaker_s_tile_for_local_speech() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        codec
            .encode(
                ServerMessage::ChatMessage {
                    author: "Rizael".to_owned(),
                    message_type: ChatMessageType::Local,
                    channel: ChannelId(0),
                    position: Some(Position::new(300, 400, 7)),
                    message: "hello".to_owned(),
                },
                &mut buf,
            )
            .unwrap();

        assert_eq!(buf[2], SRV_CHAT_MESSAGE);
        assert_eq!(u16::from_le_bytes([buf[3], buf[4]]), 6, "author length");
        assert_eq!(&buf[5..11], b"Rizael", "author");
        assert_eq!(buf[11], 0x01, "Local");
        assert_eq!(u16::from_le_bytes([buf[12], buf[13]]), 0, "channel");
        assert_eq!(buf[14], 0x01, "position present");
        assert_eq!(u16::from_le_bytes([buf[15], buf[16]]), 300, "position x");
        assert_eq!(u16::from_le_bytes([buf[17], buf[18]]), 400, "position y");
        assert_eq!(buf[19], 7, "position z");
        assert_eq!(u16::from_le_bytes([buf[20], buf[21]]), 5, "text length");
        assert_eq!(&buf[22..27], b"hello");
        assert_eq!(buf.len(), 27, "no trailing bytes");
    }

    /// Everything but local speech is spoken from nowhere the client can draw, and
    /// the flag byte is all that is sent.
    #[test]
    fn encode_chat_message_sends_no_tile_for_a_channel_line() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        codec
            .encode(
                ServerMessage::ChatMessage {
                    author: "Rizael".to_owned(),
                    message_type: ChatMessageType::Channel,
                    channel: ChannelId(7),
                    position: None,
                    message: "hello".to_owned(),
                },
                &mut buf,
            )
            .unwrap();

        assert_eq!(buf[14], 0x00, "position absent");
        assert_eq!(u16::from_le_bytes([buf[15], buf[16]]), 5, "text length");
        assert_eq!(&buf[17..22], b"hello");
        assert_eq!(
            buf.len(),
            22,
            "the None form is five bytes shorter than the Some form"
        );
    }

    /// The colour is an `Option` behind a flag byte, so the two forms differ in
    /// length. Both are asserted because a decoder that always reads three colour
    /// bytes passes the `Some` case and corrupts the `None` case.
    #[test]
    fn encode_floating_text_with_a_colour() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        codec
            .encode(
                ServerMessage::FloatingText {
                    text: "-25".to_owned(),
                    position: Position::new(0x0201, 0x0403, 7),
                    text_type: FloatingTextType::HitPoints,
                    color: Some(Color(255, 0, 64)),
                },
                &mut buf,
            )
            .unwrap();

        let payload_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        assert_eq!(
            payload_len,
            buf.len() - 2,
            "length prefix must cover the payload"
        );
        assert_eq!(buf[2], SRV_FLOATING_TEXT);
        assert_eq!(u16::from_le_bytes([buf[3], buf[4]]), 3, "text length");
        assert_eq!(&buf[5..8], b"-25");
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 0x0201, "position x");
        assert_eq!(u16::from_le_bytes([buf[10], buf[11]]), 0x0403, "position y");
        assert_eq!(buf[12], 7, "position z");
        assert_eq!(buf[13], 0x01, "HitPoints");
        assert_eq!(buf[14], 0x01, "colour present");
        assert_eq!(&buf[15..18], &[255, 0, 64], "rgb");
        assert_eq!(buf.len(), 18, "no trailing bytes");
    }

    /// The byte layout the client's `decodes_a_damaged_by` reads back. If these
    /// two disagree the mark silently stops appearing; nothing fails to compile.
    #[test]
    fn encode_damaged_by_is_an_opcode_and_one_id() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        codec
            .encode(
                ServerMessage::DamagedBy {
                    agent_id: AgentId(0x0201),
                },
                &mut buf,
            )
            .unwrap();

        let payload_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        assert_eq!(
            payload_len,
            buf.len() - 2,
            "length prefix must cover the payload"
        );
        assert_eq!(buf[2], SRV_DAMAGED_BY);
        assert_eq!(u16::from_le_bytes([buf[3], buf[4]]), 0x0201, "agent id");
        assert_eq!(buf.len(), 5, "no trailing bytes");
    }

    /// The length prefix is a *byte* count. This codebase has been bitten by
    /// byte-vs-char confusion before — `actors/session.rs` pins `max_message_length`
    /// against `"é".repeat(..)` for the same reason — and every other test here uses
    /// ASCII, where the two are indistinguishable.
    #[test]
    fn encode_floating_text_measures_the_text_in_bytes() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let text = "café"; // 4 chars, 5 bytes

        codec
            .encode(
                ServerMessage::FloatingText {
                    text: text.to_owned(),
                    position: Position::new(10, 10, 7),
                    text_type: FloatingTextType::CreatureSay,
                    color: None,
                },
                &mut buf,
            )
            .unwrap();

        assert_eq!(text.chars().count(), 4, "fixture must be multi-byte");
        assert_eq!(
            u16::from_le_bytes([buf[3], buf[4]]),
            5,
            "the prefix counts bytes, not characters"
        );
        assert_eq!(&buf[5..10], text.as_bytes());
    }

    #[test]
    fn encode_floating_text_without_a_colour() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        codec
            .encode(
                ServerMessage::FloatingText {
                    text: "hi".to_owned(),
                    position: Position::new(300, 400, 7),
                    text_type: FloatingTextType::CreatureSay,
                    color: None,
                },
                &mut buf,
            )
            .unwrap();

        assert_eq!(buf[2], SRV_FLOATING_TEXT);
        assert_eq!(u16::from_le_bytes([buf[3], buf[4]]), 2, "text length");
        assert_eq!(&buf[5..7], b"hi");
        assert_eq!(u16::from_le_bytes([buf[7], buf[8]]), 300, "position x");
        assert_eq!(u16::from_le_bytes([buf[9], buf[10]]), 400, "position y");
        assert_eq!(buf[11], 7, "position z");
        assert_eq!(buf[12], 0x02, "CreatureSay");
        assert_eq!(buf[13], 0x00, "colour absent");
        assert_eq!(
            buf.len(),
            14,
            "the None form is three bytes shorter than the Some form"
        );
    }

    #[test]
    fn encode_channel_list() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        codec
            .encode(
                ServerMessage::ChannelList {
                    channels: vec![(ChannelId(1), "World Chat".to_owned())],
                },
                &mut buf,
            )
            .unwrap();

        let payload_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        assert_eq!(payload_len, buf.len() - 2);
        assert_eq!(buf[2], SRV_CHANNEL_LIST);
        assert_eq!(u16::from_le_bytes([buf[3], buf[4]]), 1, "channel count");
        assert_eq!(u16::from_le_bytes([buf[5], buf[6]]), 1, "channel id");
        assert_eq!(u16::from_le_bytes([buf[7], buf[8]]), 10, "name length");
        assert_eq!(&buf[9..], b"World Chat");
    }

    /// `encode_channel_list` above only ever supplies one channel, so it pins the shape
    /// of a single `(id, name)` entry and the leading count but proves nothing about the
    /// loop: a `break` after the first iteration, a count field that disagrees with the
    /// number of entries actually written, or a wrong per-entry stride would all still
    /// pass it. This test walks a moving cursor across three entries with different-length
    /// names (so a fixed-stride bug can't hide) and a deliberately non-contiguous third id
    /// (so an implementation that emits a loop index instead of the real id fails too).
    #[test]
    fn encode_channel_list_writes_every_entry_in_order() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        let channels = vec![
            (ChannelId(1), "World Chat".to_owned()),
            (ChannelId(2), "Advertising".to_owned()),
            (ChannelId(7), "Help".to_owned()),
        ];

        codec
            .encode(
                ServerMessage::ChannelList {
                    channels: channels.clone(),
                },
                &mut buf,
            )
            .unwrap();

        let payload_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        assert_eq!(
            payload_len,
            buf.len() - 2,
            "length prefix must cover exactly the payload"
        );
        assert_eq!(buf[2], SRV_CHANNEL_LIST);

        fn read_u16(buf: &BytesMut, cursor: &mut usize) -> u16 {
            let value = u16::from_le_bytes([buf[*cursor], buf[*cursor + 1]]);
            *cursor += 2;
            value
        }

        let mut cursor = 3;
        let count = read_u16(&buf, &mut cursor);
        assert_eq!(count, channels.len() as u16, "channel count");

        for (expected_id, expected_name) in channels.iter() {
            let id = read_u16(&buf, &mut cursor);
            assert_eq!(id, expected_id.0, "channel id for {expected_name}");

            let name_len = read_u16(&buf, &mut cursor) as usize;
            assert_eq!(
                name_len,
                expected_name.len(),
                "name length for id {expected_id:?}"
            );

            let name_bytes = &buf[cursor..cursor + name_len];
            assert_eq!(
                name_bytes,
                expected_name.as_bytes(),
                "name bytes for id {expected_id:?}"
            );
            cursor += name_len;
        }

        assert_eq!(
            cursor,
            buf.len(),
            "buffer must be fully consumed: nothing trailing after the last name"
        );
    }

    #[test]
    fn decode_local_say_message() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let text = b"hello";
        let payload_len = (1 + 1 + text.len()) as u16;

        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&[CLI_SAY, 0x01]);
        buf.extend_from_slice(text);

        match codec.decode(&mut buf).unwrap().unwrap() {
            ClientMessage::Say { message, target } => {
                assert_eq!(message, "hello");
                assert!(matches!(target, SayTarget::Local));
            }
            other => panic!("expected Say, got {other:?}"),
        }
        assert!(buf.is_empty(), "the frame must be fully consumed");
    }

    #[test]
    fn decode_channel_say_carries_the_channel() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let text = b"hi all";
        let payload_len = (1 + 1 + 2 + text.len()) as u16;

        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&[CLI_SAY, 0x03]);
        buf.extend_from_slice(&7u16.to_le_bytes());
        buf.extend_from_slice(text);

        match codec.decode(&mut buf).unwrap().unwrap() {
            ClientMessage::Say { message, target } => {
                assert_eq!(message, "hi all");
                assert!(matches!(target, SayTarget::Channel(ChannelId(7))));
            }
            other => panic!("expected Say, got {other:?}"),
        }
        assert!(buf.is_empty(), "the frame must be fully consumed");
    }

    /// A private say names its recipient, so the message is whatever follows a
    /// length-prefixed name rather than everything after a fixed offset. The client
    /// encodes this same literal frame in `encodes_a_private_say`.
    #[test]
    fn decode_private_say_carries_the_recipient_name() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let name = b"Rizael";
        let text = b"hi";
        let payload_len = (1 + 1 + 2 + name.len() + text.len()) as u16;

        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&[CLI_SAY, 0x02]);
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name);
        buf.extend_from_slice(text);

        match codec.decode(&mut buf).unwrap().unwrap() {
            ClientMessage::Say { message, target } => {
                assert_eq!(message, "hi");
                match target {
                    SayTarget::Player(recipient) => assert_eq!(recipient, "Rizael"),
                    other => panic!("expected a player target, got {other:?}"),
                }
            }
            other => panic!("expected Say, got {other:?}"),
        }
        assert!(buf.is_empty(), "the frame must be fully consumed");
    }

    /// A name length longer than the frame must not consume the message, or the
    /// following bytes, or panic.
    #[test]
    fn a_private_say_with_an_overlong_name_length_is_rejected() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let payload_len = (1 + 1 + 2 + 2) as u16;

        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&[CLI_SAY, 0x02]);
        buf.extend_from_slice(&64u16.to_le_bytes());
        buf.extend_from_slice(b"hi");

        assert!(matches!(
            codec.decode(&mut buf),
            Err(MessageDecodeError::WrongSequence)
        ));
    }

    /// A truncated Say frame must not underflow the remaining-byte count.
    #[test]
    fn a_short_say_frame_is_rejected_without_panicking() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&[CLI_SAY, 0x03]);

        assert!(matches!(
            codec.decode(&mut buf),
            Err(MessageDecodeError::WrongSequence)
        ));
    }

    #[test]
    fn decode_unknown_chat_message_type_is_rejected() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let payload_len = (1 + 1) as u16;

        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&[CLI_SAY, 0x09]);

        assert!(matches!(
            codec.decode(&mut buf),
            Err(MessageDecodeError::WrongSequence)
        ));
    }

    #[test]
    fn decode_open_pm_chat_message() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();
        let name = b"Rizael";
        let payload_len = (1 + name.len()) as u16;

        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&[CLI_OPEN_PM_CHAT]);
        buf.extend_from_slice(name);

        match codec.decode(&mut buf).unwrap().unwrap() {
            ClientMessage::OpenPmChat { name } => assert_eq!(name, "Rizael"),
            other => panic!("expected OpenPmChat, got {other:?}"),
        }
        assert!(buf.is_empty());
    }

    /// The frame the client's `cast_spell_encodes_each_target_variant` writes, byte for byte.
    /// The param is the trailing field, so a width read wrongly here consumes the next frame
    /// rather than failing, and the two tests are what hold the pair in step.
    #[test]
    fn decode_cast_spell_message_with_and_without_a_param() {
        let mut codec = GameMessageCodec {};
        let mut buf = BytesMut::new();

        buf.extend_from_slice(&6u16.to_le_bytes());
        buf.extend_from_slice(&[CLI_CAST_SPELL, 0x04, 0x00, 0x00, 0x00, 0x00]);
        buf.extend_from_slice(&9u16.to_le_bytes());
        buf.extend_from_slice(&[
            CLI_CAST_SPELL,
            0x04,
            0x00,
            0x00,
            0x03,
            0x00,
            b'B',
            b'o',
            b'b',
        ]);

        match codec.decode(&mut buf).unwrap().unwrap() {
            ClientMessage::CastSpell {
                spell_id, param, ..
            } => {
                assert_eq!(spell_id, SpellId(4));
                assert_eq!(param, None);
            }
            other => panic!("expected CastSpell, got {other:?}"),
        }
        match codec.decode(&mut buf).unwrap().unwrap() {
            ClientMessage::CastSpell { param, .. } => assert_eq!(param.as_deref(), Some("Bob")),
            other => panic!("expected CastSpell, got {other:?}"),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn decode_channel_control_messages() {
        let mut codec = GameMessageCodec {};

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&[CLI_REQUEST_CHANNELS]);
        assert!(matches!(
            codec.decode(&mut buf).unwrap().unwrap(),
            ClientMessage::RequestChannels
        ));

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(&[CLI_OPEN_CHANNEL]);
        buf.extend_from_slice(&2u16.to_le_bytes());
        match codec.decode(&mut buf).unwrap().unwrap() {
            ClientMessage::OpenChannel { channel } => assert_eq!(channel, ChannelId(2)),
            other => panic!("expected OpenChannel, got {other:?}"),
        }

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(&[CLI_CLOSE_CHANNEL]);
        buf.extend_from_slice(&2u16.to_le_bytes());
        match codec.decode(&mut buf).unwrap().unwrap() {
            ClientMessage::CloseChannel { channel } => assert_eq!(channel, ChannelId(2)),
            other => panic!("expected CloseChannel, got {other:?}"),
        }
    }

    /// The literal frame the client's `target_lost_decodes_its_seq`
    /// (rustibia-client, src/network/messages.rs) reads. The opcode is a number on
    /// purpose: writing `SRV_TARGET_LOST` here would pin the layout and leave the
    /// opcode free to drift on one side only.
    #[test]
    fn target_lost_encodes_its_seq() {
        let mut dst = BytesMut::new();
        GameMessageCodec {}
            .encode(ServerMessage::TargetLost { seq: 77 }, &mut dst)
            .unwrap();

        assert_eq!(u16::from_le_bytes([dst[0], dst[1]]), 5);
        assert_eq!(dst[2], 23);
        assert_eq!(u32::from_le_bytes([dst[3], dst[4], dst[5], dst[6]]), 77);
    }

    /// The literal frame the client's `set_target_encodes_some_and_none`
    /// (rustibia-client, src/network/messages.rs) builds. The pair is the pin: the
    /// opcode is written as a number on purpose, so that changing the constant on
    /// one side fails a test instead of silently desyncing the wire.
    #[test]
    fn set_target_decodes_some_and_none() {
        let mut buf = BytesMut::new();
        buf.put_u16_le(7);
        buf.put_u8(17);
        buf.put_u16_le(7);
        buf.put_u32_le(5);
        let decoded = GameMessageCodec {}.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(
            decoded,
            ClientMessage::SetTarget {
                agent_id: Some(AgentId(7)),
                seq: 5
            }
        ));

        let mut buf = BytesMut::new();
        buf.put_u16_le(7);
        buf.put_u8(17);
        buf.put_u16_le(0xFFFF);
        buf.put_u32_le(6);
        let decoded = GameMessageCodec {}.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(
            decoded,
            ClientMessage::SetTarget {
                agent_id: None,
                seq: 6
            }
        ));
    }

    /// The server half of a two-sided agreement the compiler cannot hold: `position` is an
    /// anchor, and a tile draws only if `delta` names it. A single-tile effect must
    /// therefore put `(0, 0)` on the wire — the two trailing zero bytes below. Drop them
    /// and every hit splash, puff and potion silently stops rendering, with nothing failing
    /// to build on either side.
    ///
    /// Paired with the client's `core::effects::the_anchor_is_not_drawn_unless_a_delta_names_it`.
    #[test]
    fn a_single_tile_effect_names_its_own_tile() {
        let mut dst = BytesMut::new();
        GameMessageCodec {}
            .encode(
                ServerMessage::ShowEffect {
                    effect_id: EffectId(13),
                    position: Position::new(100, 200, 7),
                    delta: vec![(0, 0)],
                },
                &mut dst,
            )
            .unwrap();

        assert_eq!(
            &dst[..],
            &[
                10,
                0, // payload length
                SRV_SHOW_EFFECT,
                13,
                0, // effect 13
                100,
                0, // x
                200,
                0, // y
                7, // z
                0,
                0, // delta (0, 0) -- the anchor's own tile
            ]
        );
    }

    /// An area whose mask spares its origin (`0` in `areas.yaml`) simply leaves `(0, 0)`
    /// out. Nothing else about the frame changes, which is exactly why the rule needs a
    /// test rather than a type.
    #[test]
    fn an_area_that_spares_its_origin_omits_the_zero_delta() {
        let mut dst = BytesMut::new();
        GameMessageCodec {}
            .encode(
                ServerMessage::ShowEffect {
                    effect_id: EffectId(13),
                    position: Position::new(100, 200, 7),
                    delta: vec![(0, -1), (0, -2)],
                },
                &mut dst,
            )
            .unwrap();

        assert_eq!(
            &dst[..],
            &[
                12,
                0, // payload length
                SRV_SHOW_EFFECT,
                13,
                0, // effect 13
                100,
                0, // x
                200,
                0, // y
                7, // z
                0,
                0xFF, // (0, -1)
                0,
                0xFE, // (0, -2)
            ]
        );
    }

    #[test]
    fn player_skills_encodes_a_known_frame() {
        let mut dst = BytesMut::new();
        GameMessageCodec {}
            .encode(
                ServerMessage::PlayerSkills {
                    experience: 4231,
                    skills: vec![(
                        SkillType::Level,
                        SkillProgress {
                            level: 8,
                            percent_bp: 4321,
                        },
                    )],
                },
                &mut dst,
            )
            .unwrap();

        assert_eq!(
            &dst[..],
            &[
                15,
                0, // payload length
                SRV_PLAYER_SKILLS,
                0x87,
                0x10,
                0,
                0,
                0,
                0,
                0,
                0, // experience 4231
                1, // one row
                0, // SkillType::Level
                8,
                0, // level
                0xE1,
                0x10, // 43.21%
            ]
        );
    }

    /// The two pools and the four outfit colours are written as bare sequences of numbers, so a
    /// transposed pair compiles and simply reports the wrong thing for ever: swapped pool halves
    /// show a full life bar on a dying player, swapped colours redress every character. The
    /// client reads this frame positionally and nothing links the two sides — see the vault's
    /// `two-sided-agreements`.
    #[test]
    fn describe_player_encodes_its_pools_and_outfit_in_order() {
        let mut dst = BytesMut::new();
        let no_item = || None;
        GameMessageCodec {}
            .encode(
                ServerMessage::DescribePlayer {
                    agent_id: AgentId(7),
                    position: Position::new(0x0102, 0x0304, 5),
                    facing: Facing::North,
                    name: "Ab".to_owned(),
                    level: 9,
                    life: Pool {
                        current: 30,
                        maximum: 40,
                    },
                    mana: Pool {
                        current: 50,
                        maximum: 60,
                    },
                    outfit: (OutfitId(0x0201), OutfitColors::new(11, 22, 33, 44)),
                    speed: 120,
                    capacity: 0,
                    inventory_head: no_item(),
                    inventory_amulet: no_item(),
                    inventory_backpack: no_item(),
                    inventory_chest: no_item(),
                    inventory_right_hand: no_item(),
                    inventory_left_hand: no_item(),
                    inventory_legs: no_item(),
                    inventory_feet: no_item(),
                    inventory_ring: no_item(),
                    inventory_trinket: no_item(),
                },
                &mut dst,
            )
            .unwrap();

        // Past the length prefix, the opcode, the agent id, the position and the facing.
        let after_facing = 2 + 1 + 2 + 5 + 1;
        let name = &dst[after_facing..after_facing + 4];
        assert_eq!(name, &[2, 0, b'A', b'b'], "name is u16-length-prefixed");

        let rest = &dst[after_facing + 4..];
        assert_eq!(&rest[..2], &[9, 0], "level precedes the pools");
        assert_eq!(
            &rest[2..18],
            &[30, 0, 0, 0, 40, 0, 0, 0, 50, 0, 0, 0, 60, 0, 0, 0],
            "life current, life maximum, mana current, mana maximum"
        );
        assert_eq!(
            &rest[18..24],
            &[0x01, 0x02, 11, 22, 33, 44],
            "outfit id, then head, body, legs, feet"
        );
    }

    /// The one string the wire length-prefixes with a `u8` rather than a `u16`.
    #[test]
    fn a_container_title_is_length_prefixed_with_a_single_byte() {
        let mut dst = BytesMut::new();
        GameMessageCodec {}
            .encode(
                ServerMessage::OpenContainer {
                    container_id: ContainerId(3),
                    capacity: 8,
                    has_parent: false,
                    title: "bag".to_owned(),
                    items: Box::new([]),
                },
                &mut dst,
            )
            .unwrap();

        let payload_len = u16::from_le_bytes([dst[0], dst[1]]) as usize;
        assert_eq!(payload_len, dst.len() - 2);
        assert_eq!(
            &dst[2..2 + 1 + 2 + 1 + 1 + 4],
            &[
                SRV_OPEN_CONTAINER,
                3,
                0, // container id
                8, // capacity
                0, // has_parent
                3,
                b'b',
                b'a',
                b'g', // one length byte, then the title
            ]
        );
    }

    #[test]
    fn skill_changed_encodes_a_known_frame() {
        let mut dst = BytesMut::new();
        GameMessageCodec {}
            .encode(
                ServerMessage::SkillUpdated {
                    skill: SkillType::Sword,
                    progress: SkillProgress {
                        level: 12,
                        percent_bp: 4909,
                    },
                },
                &mut dst,
            )
            .unwrap();

        assert_eq!(
            &dst[..],
            &[
                6,
                0, // payload length
                SRV_SKILL_UPDATED,
                3, // SkillType::Sword
                12,
                0, // level
                0x2D,
                0x13, // 49.09%
            ]
        );
    }

    #[test]
    fn experience_changed_encodes_a_known_frame() {
        let mut dst = BytesMut::new();
        GameMessageCodec {}
            .encode(
                ServerMessage::ExperienceUpdated { experience: 4231 },
                &mut dst,
            )
            .unwrap();

        assert_eq!(
            &dst[..],
            &[
                9,
                0, // payload length
                SRV_EXPERIENCE_UPDATED,
                0x87,
                0x10,
                0,
                0,
                0,
                0,
                0,
                0,
            ]
        );
    }

    #[test]
    fn spell_list_encodes_a_known_frame() {
        let mut dst = BytesMut::new();
        GameMessageCodec {}
            .encode(
                ServerMessage::SpellList {
                    spells: vec![SpellListEntry {
                        id: SpellId(4),
                        name: "Ab".to_owned(),
                        words: "cd".to_owned(),
                        level: 12,
                        icon: 29,
                        aimable: true,
                        group: SpellGroup::Support,
                    }],
                },
                &mut dst,
            )
            .unwrap();

        assert_eq!(
            &dst[..],
            &[
                19,
                0, // payload length
                SRV_SPELL_LIST,
                1,
                0, // one spell
                4,
                0, // id
                2,
                0,
                b'A',
                b'b', // name
                2,
                0,
                b'c',
                b'd', // words
                12,
                0, // level
                29,
                0, // icon
                1, // aimable
                2, // group
            ]
        );
    }
}
