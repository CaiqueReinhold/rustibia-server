use rustibia_server::constants::view::{MAX_VISIBLE_ITEMS, VIEWPORT_SIZE};
use rustibia_server::entities::{
    agent::{AgentId, Facing, OutfitColors, OutfitId, Pool},
    chat::{ChannelId, ChatMessageType, SayTarget},
    effects::{EffectId, MissileId},
    inventory::InventorySlot,
    items::{ClientItemRef, ContainerId, ItemId},
    position::{Direction, Position},
    skills::SkillType,
    spells::{SpellGroup, SpellId, SpellTarget},
};
use rustibia_server::messages::{
    ClientMessage, Color, FloatingTextType, ItemStack, ServerMessage, SkillProgress,
    SpellListEntry, TextMessageType,
};
use tokio_util::bytes::{Buf, BufMut, BytesMut, TryGetError};
use tokio_util::codec::{Decoder, Encoder};

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("malformed frame (opcode {opcode})")]
    Malformed { opcode: u8 },
    #[error("unknown opcode {0}")]
    UnknownOpcode(u8),
}

impl From<TryGetError> for WireError {
    fn from(_: TryGetError) -> Self {
        WireError::Malformed { opcode: 0 }
    }
}

impl WireError {
    /// The opcode is unknown at the point a primitive read fails; `decode_message`
    /// is the one place that knows it, and corrects it here once the failure bubbles up.
    fn with_opcode(self, opcode: u8) -> Self {
        match self {
            WireError::Malformed { .. } => WireError::Malformed { opcode },
            other => other,
        }
    }
}

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

#[derive(Default)]
pub struct LoadtestCodec {
    bytes_read: u64,
}

impl LoadtestCodec {
    pub(crate) fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

fn encode_position(pos: &Position, dst: &mut BytesMut) {
    dst.put_u16_le(pos.x);
    dst.put_u16_le(pos.y);
    dst.put_u8(pos.z);
}

fn encode_direction(direction: Direction) -> u8 {
    match direction {
        Direction::North => 0x00,
        Direction::East => 0x01,
        Direction::West => 0x02,
        Direction::South => 0x03,
        Direction::NorthEast => 0x04,
        Direction::NorthWest => 0x05,
        Direction::SouthEast => 0x06,
        Direction::SouthWest => 0x07,
    }
}

fn encode_facing(facing: Facing) -> u8 {
    match facing {
        Facing::North => 1,
        Facing::East => 2,
        Facing::South => 3,
        Facing::West => 4,
    }
}

fn encode_item_ref(item: &ClientItemRef, dst: &mut BytesMut) {
    encode_position(&item.position, dst);
    dst.put_u16_le(item.item_id.0);
    dst.put_u8(item.stack_index);
}

fn encode_optional_agent(agent_id: Option<AgentId>, dst: &mut BytesMut) {
    dst.put_u16_le(agent_id.map_or(0xFFFF, |id| id.0));
}

fn encode_string(s: &str, dst: &mut BytesMut) {
    let bytes = s.as_bytes();
    dst.put_u16_le(bytes.len() as u16);
    dst.put_slice(bytes);
}

fn encode_param(param: &Option<String>, dst: &mut BytesMut) {
    match param {
        Some(s) => encode_string(s, dst),
        None => dst.put_u16_le(0),
    }
}

fn encode_spell_target(target: &SpellTarget, dst: &mut BytesMut) {
    match target {
        SpellTarget::None => dst.put_u8(0x00),
        SpellTarget::Agent(agent_id) => {
            dst.put_u8(0x01);
            dst.put_u16_le(agent_id.0);
        }
        SpellTarget::Position(position) => {
            dst.put_u8(0x02);
            encode_position(position, dst);
        }
    }
}

impl Encoder<ClientMessage> for LoadtestCodec {
    type Error = WireError;

    fn encode(&mut self, item: ClientMessage, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let len_offset = dst.len();
        dst.put_u16_le(0);

        match item {
            ClientMessage::Ping => dst.put_u8(CLI_PING),
            ClientMessage::Login { auth_token } => {
                dst.put_u8(CLI_LOGIN);
                dst.put_slice(auth_token.as_bytes());
            }
            ClientMessage::MovePlayer { direction } => {
                dst.put_u8(CLI_MOVE_PLAYER);
                dst.put_u8(encode_direction(direction));
            }
            ClientMessage::GetPlayerPosition => dst.put_u8(CLI_GET_PLAYER_POS),
            ClientMessage::MoveItem { item, amount, to } => {
                dst.put_u8(CLI_MOVE_ITEM);
                encode_position(&item.position, dst);
                dst.put_u16_le(item.item_id.0);
                dst.put_u8(amount);
                dst.put_u8(item.stack_index);
                encode_position(&to, dst);
            }
            ClientMessage::UseItem { item } => {
                dst.put_u8(CLI_USE_ITEM);
                encode_item_ref(&item, dst);
            }
            ClientMessage::CloseContainer { container_id } => {
                dst.put_u8(CLI_CLOSE_CONTAINER);
                dst.put_u16_le(container_id.0);
            }
            ClientMessage::OpenParentContainer { container_id } => {
                dst.put_u8(CLI_OPEN_PARENT_CONTAINER);
                dst.put_u16_le(container_id.0);
            }
            ClientMessage::ChangeDirection { direction } => {
                dst.put_u8(CLI_CHANGE_DIRECTION);
                dst.put_u8(encode_facing(direction));
            }
            ClientMessage::Logout => dst.put_u8(CLI_LOGOUT),
            ClientMessage::UseItemWith {
                source,
                target,
                target_agent,
            } => {
                dst.put_u8(CLI_USE_ITEM_WITH);
                encode_item_ref(&source, dst);
                encode_item_ref(&target, dst);
                encode_optional_agent(target_agent, dst);
            }
            ClientMessage::Look { position } => {
                dst.put_u8(CLI_LOOK);
                encode_position(&position, dst);
            }
            ClientMessage::Say { message, target } => {
                dst.put_u8(CLI_SAY);
                match target {
                    SayTarget::Local => dst.put_u8(0x01),
                    SayTarget::Channel(channel) => {
                        dst.put_u8(0x03);
                        dst.put_u16_le(channel.0);
                    }
                    SayTarget::Player(name) => {
                        dst.put_u8(0x02);
                        encode_string(&name, dst);
                    }
                }
                dst.put_slice(message.as_bytes());
            }
            ClientMessage::RequestChannels => dst.put_u8(CLI_REQUEST_CHANNELS),
            ClientMessage::OpenChannel { channel } => {
                dst.put_u8(CLI_OPEN_CHANNEL);
                dst.put_u16_le(channel.0);
            }
            ClientMessage::CloseChannel { channel } => {
                dst.put_u8(CLI_CLOSE_CHANNEL);
                dst.put_u16_le(channel.0);
            }
            ClientMessage::OpenPmChat { name } => {
                dst.put_u8(CLI_OPEN_PM_CHAT);
                dst.put_slice(name.as_bytes());
            }
            ClientMessage::SetTarget { agent_id, seq } => {
                dst.put_u8(CLI_SET_TARGET);
                encode_optional_agent(agent_id, dst);
                dst.put_u32_le(seq);
            }
            ClientMessage::CastSpell {
                spell_id,
                target,
                param,
            } => {
                dst.put_u8(CLI_CAST_SPELL);
                dst.put_u16_le(spell_id.0);
                encode_spell_target(&target, dst);
                encode_param(&param, dst);
            }
        }

        let payload_len = (dst.len() - len_offset - 2) as u16;
        dst[len_offset..len_offset + 2].copy_from_slice(&payload_len.to_le_bytes());
        Ok(())
    }
}

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
const SRV_INVENTORY_SLOT_UPDATED: u8 = 12;
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

fn read_utf8(buf: &mut BytesMut, len: usize) -> Result<String, WireError> {
    if buf.len() < len {
        return Err(WireError::Malformed { opcode: 0 });
    }
    String::from_utf8(buf.split_to(len).to_vec()).map_err(|_| WireError::Malformed { opcode: 0 })
}

fn read_string(buf: &mut BytesMut) -> Result<String, WireError> {
    let len = buf.try_get_u16_le()? as usize;
    read_utf8(buf, len)
}

fn read_short_string(buf: &mut BytesMut) -> Result<String, WireError> {
    let len = buf.try_get_u8()? as usize;
    read_utf8(buf, len)
}

fn read_position(buf: &mut BytesMut) -> Result<Position, WireError> {
    Ok(Position {
        x: buf.try_get_u16_le()?,
        y: buf.try_get_u16_le()?,
        z: buf.try_get_u8()?,
    })
}

fn read_direction(buf: &mut BytesMut) -> Result<Direction, WireError> {
    match buf.try_get_u8()? {
        0x00 => Ok(Direction::North),
        0x01 => Ok(Direction::East),
        0x02 => Ok(Direction::West),
        0x03 => Ok(Direction::South),
        0x04 => Ok(Direction::NorthEast),
        0x05 => Ok(Direction::NorthWest),
        0x06 => Ok(Direction::SouthEast),
        0x07 => Ok(Direction::SouthWest),
        _ => Err(WireError::Malformed { opcode: 0 }),
    }
}

fn read_facing(buf: &mut BytesMut) -> Result<Facing, WireError> {
    match buf.try_get_u8()? {
        1 => Ok(Facing::North),
        2 => Ok(Facing::East),
        3 => Ok(Facing::South),
        4 => Ok(Facing::West),
        _ => Err(WireError::Malformed { opcode: 0 }),
    }
}

fn read_chat_message_type(buf: &mut BytesMut) -> Result<ChatMessageType, WireError> {
    match buf.try_get_u8()? {
        0x01 => Ok(ChatMessageType::Local),
        0x02 => Ok(ChatMessageType::Private),
        0x03 => Ok(ChatMessageType::Channel),
        _ => Err(WireError::Malformed { opcode: 0 }),
    }
}

fn read_text_message_type(buf: &mut BytesMut) -> Result<TextMessageType, WireError> {
    match buf.try_get_u8()? {
        0x01 => Ok(TextMessageType::ActionDenied),
        0x02 => Ok(TextMessageType::Look),
        _ => Err(WireError::Malformed { opcode: 0 }),
    }
}

fn read_floating_text_type(buf: &mut BytesMut) -> Result<FloatingTextType, WireError> {
    match buf.try_get_u8()? {
        0x01 => Ok(FloatingTextType::HitPoints),
        0x02 => Ok(FloatingTextType::CreatureSay),
        _ => Err(WireError::Malformed { opcode: 0 }),
    }
}

fn read_spell_group(buf: &mut BytesMut) -> Result<SpellGroup, WireError> {
    match buf.try_get_u8()? {
        0 => Ok(SpellGroup::Attack),
        1 => Ok(SpellGroup::Healing),
        2 => Ok(SpellGroup::Support),
        _ => Err(WireError::Malformed { opcode: 0 }),
    }
}

fn read_optional_item(buf: &mut BytesMut) -> Result<Option<ItemId>, WireError> {
    let raw = buf.try_get_u16_le()?;
    Ok(if raw == 0xFFFF {
        None
    } else {
        Some(ItemId(raw))
    })
}

fn read_pool(buf: &mut BytesMut) -> Result<Pool, WireError> {
    Ok(Pool {
        current: buf.try_get_u32_le()?,
        maximum: buf.try_get_u32_le()?,
    })
}

fn read_outfit(buf: &mut BytesMut) -> Result<(OutfitId, OutfitColors), WireError> {
    let id = OutfitId(buf.try_get_u16_le()?);
    let colors = OutfitColors::new(
        buf.try_get_u8()?,
        buf.try_get_u8()?,
        buf.try_get_u8()?,
        buf.try_get_u8()?,
    );
    Ok((id, colors))
}

fn read_tile_entries(buf: &mut BytesMut) -> Result<Vec<(ItemId, u8)>, WireError> {
    let mut items = Vec::new();
    loop {
        let id = buf.try_get_u16_le()?;
        if id == 0xFFFF {
            break;
        }
        let amount = buf.try_get_u8()?;
        items.push((ItemId(id), amount));
    }
    Ok(items)
}

fn read_item_stack(buf: &mut BytesMut) -> Result<ItemStack, WireError> {
    let entries = read_tile_entries(buf)?;
    if entries.len() > MAX_VISIBLE_ITEMS {
        return Err(WireError::Malformed { opcode: 0 });
    }
    let mut stack: ItemStack = Default::default();
    for (slot, entry) in stack.iter_mut().zip(entries) {
        *slot = Some(entry);
    }
    Ok(stack)
}

type ContainerItems = Box<[Option<(ItemId, u8)>]>;

fn read_container_items(buf: &mut BytesMut) -> Result<ContainerItems, WireError> {
    Ok(read_tile_entries(buf)?
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>()
        .into_boxed_slice())
}

impl Decoder for LoadtestCodec {
    type Item = ServerMessage;
    type Error = WireError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if buf.len() < 2 {
            return Ok(None);
        }

        let payload_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;

        if buf.len() < 2 + payload_len {
            return Ok(None);
        }

        self.bytes_read += 2 + payload_len as u64;

        buf.advance(2);
        let mut payload = buf.split_to(payload_len);
        let opcode = payload.first().copied().unwrap_or(0);
        let message = decode_message(&mut payload)?;

        if !payload.is_empty() {
            return Err(WireError::Malformed { opcode });
        }

        Ok(Some(message))
    }
}

fn decode_message(payload: &mut BytesMut) -> Result<ServerMessage, WireError> {
    let opcode = payload.try_get_u8()?;
    decode_body(opcode, payload).map_err(|err| err.with_opcode(opcode))
}

fn decode_body(opcode: u8, payload: &mut BytesMut) -> Result<ServerMessage, WireError> {
    match opcode {
        SRV_PONG => Ok(ServerMessage::Pong),
        SRV_LOGIN_ERROR => Ok(ServerMessage::LoginError),
        SRV_DESCRIBE_PLAYER => {
            let agent_id = AgentId(payload.try_get_u16_le()?);
            let position = read_position(payload)?;
            let facing = read_facing(payload)?;
            let name = read_string(payload)?;
            let level = payload.try_get_u16_le()?;
            let life = read_pool(payload)?;
            let mana = read_pool(payload)?;
            let outfit = read_outfit(payload)?;
            let speed = payload.try_get_u16_le()?;
            let capacity = payload.try_get_u32_le()?;
            Ok(ServerMessage::DescribePlayer {
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
                inventory_head: read_optional_item(payload)?,
                inventory_amulet: read_optional_item(payload)?,
                inventory_backpack: read_optional_item(payload)?,
                inventory_chest: read_optional_item(payload)?,
                inventory_right_hand: read_optional_item(payload)?,
                inventory_left_hand: read_optional_item(payload)?,
                inventory_legs: read_optional_item(payload)?,
                inventory_feet: read_optional_item(payload)?,
                inventory_ring: read_optional_item(payload)?,
                inventory_trinket: read_optional_item(payload)?,
            })
        }
        SRV_DESCRIBE_MAP => {
            let center = read_position(payload)?;
            let floor = payload.try_get_u8()?;
            let mut tiles: Box<[ItemStack; VIEWPORT_SIZE]> =
                Box::new(std::array::from_fn(|_| ItemStack::default()));
            for tile in tiles.iter_mut() {
                *tile = read_item_stack(payload)?;
            }
            Ok(ServerMessage::DescribeMap {
                tiles,
                center,
                floor,
            })
        }
        SRV_TILE_UPDATED => {
            let position = read_position(payload)?;
            let items = Box::new(read_item_stack(payload)?);
            Ok(ServerMessage::TileUpdated { position, items })
        }
        SRV_PLAYER_WALK_ACK => {
            let position = read_position(payload)?;
            let mut tiles = Vec::new();
            loop {
                let floor = payload.try_get_u8()?;
                if floor == 0xFF {
                    break;
                }
                let count = payload.try_get_u8()?;
                let mut floor_tiles = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    floor_tiles.push(read_item_stack(payload)?);
                }
                tiles.push((floor, floor_tiles.into_boxed_slice()));
            }
            Ok(ServerMessage::PlayerWalkAck { position, tiles })
        }
        SRV_PLAYER_POS => Ok(ServerMessage::PlayerPosition {
            position: read_position(payload)?,
        }),
        SRV_TEXT_MESSAGE => {
            let text = read_string(payload)?;
            let message_type = read_text_message_type(payload)?;
            Ok(ServerMessage::TextMessage { text, message_type })
        }
        SRV_OPEN_CONTAINER => {
            let container_id = ContainerId(payload.try_get_u16_le()?);
            let capacity = payload.try_get_u8()?;
            let has_parent = payload.try_get_u8()? != 0;
            let title = read_short_string(payload)?;
            let items = read_container_items(payload)?;
            Ok(ServerMessage::OpenContainer {
                container_id,
                capacity,
                has_parent,
                title,
                items,
            })
        }
        SRV_UPDATE_CONTAINER => {
            let container_id = ContainerId(payload.try_get_u16_le()?);
            let items = read_container_items(payload)?;
            Ok(ServerMessage::UpdateContainer {
                container_id,
                items,
            })
        }
        SRV_CONTAINER_CLOSED => Ok(ServerMessage::ContainerClosed {
            container_id: ContainerId(payload.try_get_u16_le()?),
        }),
        SRV_PLAYER_WALK_DENIED => Ok(ServerMessage::PlayerWalkDenied),
        SRV_INVENTORY_SLOT_UPDATED => {
            let slot = InventorySlot::from_id(payload.try_get_u8()?)
                .ok_or(WireError::Malformed { opcode: 0 })?;
            let item_id = read_optional_item(payload)?;
            Ok(ServerMessage::IventorySlotUpdated { slot, item_id })
        }
        SRV_PLAYER_CAPACITY_UPDATED => Ok(ServerMessage::PlayerCapacityUpdated {
            cap: payload.try_get_u32_le()?,
        }),
        SRV_AGENT_DIRECTION_CHANGED => {
            let agent_id = AgentId(payload.try_get_u16_le()?);
            let facing = read_facing(payload)?;
            Ok(ServerMessage::AgentChangedDirection { agent_id, facing })
        }
        SRV_REMOVE_AGENT => Ok(ServerMessage::RemoveAgent {
            agent_id: AgentId(payload.try_get_u16_le()?),
        }),
        SRV_MOVE_AGENT => {
            let agent_id = AgentId(payload.try_get_u16_le()?);
            let direction = read_direction(payload)?;
            let from = read_position(payload)?;
            Ok(ServerMessage::MoveAgent {
                agent_id,
                direction,
                from,
            })
        }
        SRV_SPAWN_AGENT => {
            let agent_id = AgentId(payload.try_get_u16_le()?);
            let position = read_position(payload)?;
            let facing = read_facing(payload)?;
            let name = read_string(payload)?;
            let life = payload.try_get_u32_le()?;
            let outfit = read_outfit(payload)?;
            let speed = payload.try_get_u16_le()?;
            Ok(ServerMessage::SpawnAgent {
                agent_id,
                outfit,
                position,
                facing,
                name,
                life,
                speed,
            })
        }
        SRV_TELEPORT_AGENT => {
            let agent_id = AgentId(payload.try_get_u16_le()?);
            let position = read_position(payload)?;
            Ok(ServerMessage::TeleportAgent { agent_id, position })
        }
        SRV_CHAT_MESSAGE => {
            let author = read_string(payload)?;
            let message_type = read_chat_message_type(payload)?;
            let channel = ChannelId(payload.try_get_u16_le()?);
            let position = match payload.try_get_u8()? {
                0x01 => Some(read_position(payload)?),
                0x00 => None,
                _ => return Err(WireError::Malformed { opcode: 0 }),
            };
            let message = read_string(payload)?;
            Ok(ServerMessage::ChatMessage {
                author,
                message_type,
                channel,
                position,
                message,
            })
        }
        SRV_CHANNEL_LIST => {
            let count = payload.try_get_u16_le()?;
            let mut channels = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let id = ChannelId(payload.try_get_u16_le()?);
                let name = read_string(payload)?;
                channels.push((id, name));
            }
            Ok(ServerMessage::ChannelList { channels })
        }
        SRV_PRIVATE_CHAT_OPENED => Ok(ServerMessage::PrivateChatOpened {
            name: read_string(payload)?,
        }),
        SRV_FLOATING_TEXT => {
            let text = read_string(payload)?;
            let position = read_position(payload)?;
            let text_type = read_floating_text_type(payload)?;
            let color = match payload.try_get_u8()? {
                0x01 => Some(Color(
                    payload.try_get_u8()?,
                    payload.try_get_u8()?,
                    payload.try_get_u8()?,
                )),
                0x00 => None,
                _ => return Err(WireError::Malformed { opcode: 0 }),
            };
            Ok(ServerMessage::FloatingText {
                text,
                position,
                text_type,
                color,
            })
        }
        SRV_TARGET_LOST => Ok(ServerMessage::TargetLost {
            seq: payload.try_get_u32_le()?,
        }),
        SRV_DAMAGED_BY => Ok(ServerMessage::DamagedBy {
            agent_id: AgentId(payload.try_get_u16_le()?),
        }),
        SRV_AGENT_LIFE_UPDATED => {
            let agent_id = AgentId(payload.try_get_u16_le()?);
            let current = payload.try_get_u32_le()?;
            let max = payload.try_get_u32_le()?;
            Ok(ServerMessage::AgentLifeChanged {
                agent_id,
                current,
                max,
            })
        }
        SRV_SHOW_EFFECT => {
            let effect_id = EffectId(payload.try_get_u16_le()?);
            let position = read_position(payload)?;
            let mut delta = Vec::new();
            while !payload.is_empty() {
                let dx = payload.try_get_i8()?;
                let dy = payload.try_get_i8()?;
                delta.push((dx, dy));
            }
            Ok(ServerMessage::ShowEffect {
                effect_id,
                position,
                delta,
            })
        }
        SRV_LAUNCH_MISSILE => {
            let from = read_position(payload)?;
            let to = read_position(payload)?;
            let missile_id = MissileId(payload.try_get_u16_le()?);
            Ok(ServerMessage::LaunchMissile {
                from,
                to,
                missile_id,
            })
        }
        SRV_AGENT_MANA_UPDATED => {
            let agent_id = AgentId(payload.try_get_u16_le()?);
            let current = payload.try_get_u32_le()?;
            let max = payload.try_get_u32_le()?;
            Ok(ServerMessage::AgentManaUpdated {
                agent_id,
                current,
                max,
            })
        }
        SRV_PLAYER_SKILLS => {
            let experience = payload.try_get_u64_le()?;
            let count = payload.try_get_u8()?;
            let mut skills = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let skill = SkillType::from_id(payload.try_get_u8()?)
                    .ok_or(WireError::Malformed { opcode: 0 })?;
                let level = payload.try_get_u16_le()?;
                let percent_bp = payload.try_get_u16_le()?;
                skills.push((skill, SkillProgress { level, percent_bp }));
            }
            Ok(ServerMessage::PlayerSkills { experience, skills })
        }
        SRV_SKILL_UPDATED => {
            let skill = SkillType::from_id(payload.try_get_u8()?)
                .ok_or(WireError::Malformed { opcode: 0 })?;
            let level = payload.try_get_u16_le()?;
            let percent_bp = payload.try_get_u16_le()?;
            Ok(ServerMessage::SkillUpdated {
                skill,
                progress: SkillProgress { level, percent_bp },
            })
        }
        SRV_EXPERIENCE_UPDATED => Ok(ServerMessage::ExperienceUpdated {
            experience: payload.try_get_u64_le()?,
        }),
        SRV_SPELL_CAST => {
            let spell = SpellId(payload.try_get_u16_le()?);
            let spell_cooldown_ms = payload.try_get_u32_le()?;
            let group_cooldown_ms = payload.try_get_u32_le()?;
            Ok(ServerMessage::SpellCast {
                spell,
                spell_cooldown_ms,
                group_cooldown_ms,
            })
        }
        SRV_SPELL_LIST => {
            let count = payload.try_get_u16_le()?;
            let mut spells = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let id = SpellId(payload.try_get_u16_le()?);
                let name = read_string(payload)?;
                let words = read_string(payload)?;
                let level = payload.try_get_u16_le()?;
                let icon = payload.try_get_u16_le()?;
                let aimable = payload.try_get_u8()? != 0;
                let group = read_spell_group(payload)?;
                spells.push(SpellListEntry {
                    id,
                    name,
                    words,
                    level,
                    icon,
                    aimable,
                    group,
                });
            }
            Ok(ServerMessage::SpellList { spells })
        }
        SRV_AGENT_SPEED_UPDATED => {
            let agent_id = AgentId(payload.try_get_u16_le()?);
            let speed = payload.try_get_u16_le()?;
            Ok(ServerMessage::AgentSpeedUpdated { agent_id, speed })
        }
        SRV_PLAYER_STATUS => Ok(ServerMessage::PlayerStatus {
            status: payload.try_get_u32_le()?,
        }),
        _ => Err(WireError::UnknownOpcode(opcode)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustibia_server::entities::{
        agent::{AgentId, Facing},
        chat::{ChannelId, SayTarget},
        items::{ClientItemRef, ContainerId, ItemId},
        position::{Direction, Position},
        spells::{SpellId, SpellTarget},
    };
    use rustibia_server::messages::GameMessageCodec as ServerCodec;
    use tokio_util::codec::Decoder;

    const CLIENT_MESSAGE_VARIANTS: usize = 19;

    /// Exhaustive on purpose: a new `ClientMessage` variant fails to compile here
    /// until it is given an index. `every_client_message_variant_has_a_sample`
    /// is what then catches a variant with an index but no sample.
    fn client_message_variant_index(message: &ClientMessage) -> usize {
        match message {
            ClientMessage::Ping => 0,
            ClientMessage::Login { .. } => 1,
            ClientMessage::MovePlayer { .. } => 2,
            ClientMessage::GetPlayerPosition => 3,
            ClientMessage::MoveItem { .. } => 4,
            ClientMessage::UseItem { .. } => 5,
            ClientMessage::CloseContainer { .. } => 6,
            ClientMessage::OpenParentContainer { .. } => 7,
            ClientMessage::ChangeDirection { .. } => 8,
            ClientMessage::Logout => 9,
            ClientMessage::UseItemWith { .. } => 10,
            ClientMessage::Look { .. } => 11,
            ClientMessage::Say { .. } => 12,
            ClientMessage::RequestChannels => 13,
            ClientMessage::OpenChannel { .. } => 14,
            ClientMessage::CloseChannel { .. } => 15,
            ClientMessage::OpenPmChat { .. } => 16,
            ClientMessage::SetTarget { .. } => 17,
            ClientMessage::CastSpell { .. } => 18,
        }
    }

    fn every_client_message() -> Vec<ClientMessage> {
        let item = ClientItemRef {
            position: Position::new(100, 200, 7),
            item_id: ItemId(266),
            stack_index: 1,
        };

        vec![
            ClientMessage::Ping,
            ClientMessage::Login {
                auth_token: "token-abc".to_string(),
            },
            ClientMessage::MovePlayer {
                direction: Direction::NorthEast,
            },
            ClientMessage::GetPlayerPosition,
            ClientMessage::MoveItem {
                item: item.clone(),
                amount: 3,
                to: Position::new(101, 200, 7),
            },
            ClientMessage::UseItem { item: item.clone() },
            ClientMessage::CloseContainer {
                container_id: ContainerId(2),
            },
            ClientMessage::OpenParentContainer {
                container_id: ContainerId(2),
            },
            ClientMessage::ChangeDirection {
                direction: Facing::West,
            },
            ClientMessage::Logout,
            ClientMessage::UseItemWith {
                source: item.clone(),
                target: item.clone(),
                target_agent: Some(AgentId(9)),
            },
            ClientMessage::Look {
                position: Position::new(5, 6, 7),
            },
            ClientMessage::Say {
                message: "hello there".to_string(),
                target: SayTarget::Local,
            },
            ClientMessage::Say {
                message: "on a channel".to_string(),
                target: SayTarget::Channel(ChannelId(1)),
            },
            ClientMessage::Say {
                message: "privately".to_string(),
                target: SayTarget::Player("Someone".to_string()),
            },
            ClientMessage::RequestChannels,
            ClientMessage::OpenChannel {
                channel: ChannelId(3),
            },
            ClientMessage::CloseChannel {
                channel: ChannelId(3),
            },
            ClientMessage::OpenPmChat {
                name: "Someone".to_string(),
            },
            ClientMessage::SetTarget {
                agent_id: Some(AgentId(9)),
                seq: 42,
            },
            ClientMessage::SetTarget {
                agent_id: None,
                seq: 43,
            },
            ClientMessage::CastSpell {
                spell_id: SpellId(1),
                target: SpellTarget::None,
                param: None,
            },
            ClientMessage::CastSpell {
                spell_id: SpellId(2),
                target: SpellTarget::Agent(AgentId(9)),
                param: Some("Someone".to_string()),
            },
            ClientMessage::CastSpell {
                spell_id: SpellId(3),
                target: SpellTarget::Position(Position::new(5, 6, 7)),
                param: None,
            },
        ]
    }

    #[test]
    fn every_client_message_variant_has_a_sample() {
        let indices: std::collections::BTreeSet<usize> = every_client_message()
            .iter()
            .map(client_message_variant_index)
            .collect();

        assert_eq!(indices, (0..CLIENT_MESSAGE_VARIANTS).collect());
    }

    /// The server's own decoder reads back what this codec writes, so a
    /// layout or opcode divergence between the two halves fails here rather than
    /// silently desyncing a run.
    #[test]
    fn the_server_decodes_everything_this_codec_encodes() {
        for message in every_client_message() {
            let mut buf = BytesMut::new();
            LoadtestCodec::default()
                .encode(message.clone(), &mut buf)
                .expect("encoding");

            let decoded = ServerCodec {}.decode(&mut buf).expect("decoding");

            assert_eq!(decoded, Some(message), "round trip");
            assert!(buf.is_empty(), "the frame left trailing bytes");
        }
    }

    const SERVER_MESSAGE_VARIANTS: usize = 36;

    /// Exhaustive for the same reason as `client_message_variant_index`.
    fn server_message_variant_index(message: &ServerMessage) -> usize {
        match message {
            ServerMessage::Pong => 0,
            ServerMessage::LoginError => 1,
            ServerMessage::DescribePlayer { .. } => 2,
            ServerMessage::DescribeMap { .. } => 3,
            ServerMessage::TileUpdated { .. } => 4,
            ServerMessage::PlayerWalkAck { .. } => 5,
            ServerMessage::PlayerPosition { .. } => 6,
            ServerMessage::TextMessage { .. } => 7,
            ServerMessage::OpenContainer { .. } => 8,
            ServerMessage::UpdateContainer { .. } => 9,
            ServerMessage::ContainerClosed { .. } => 10,
            ServerMessage::PlayerWalkDenied => 11,
            ServerMessage::IventorySlotUpdated { .. } => 12,
            ServerMessage::PlayerCapacityUpdated { .. } => 13,
            ServerMessage::AgentChangedDirection { .. } => 14,
            ServerMessage::RemoveAgent { .. } => 15,
            ServerMessage::MoveAgent { .. } => 16,
            ServerMessage::SpawnAgent { .. } => 17,
            ServerMessage::TeleportAgent { .. } => 18,
            ServerMessage::ChatMessage { .. } => 19,
            ServerMessage::ChannelList { .. } => 20,
            ServerMessage::PrivateChatOpened { .. } => 21,
            ServerMessage::FloatingText { .. } => 22,
            ServerMessage::TargetLost { .. } => 23,
            ServerMessage::DamagedBy { .. } => 24,
            ServerMessage::ShowEffect { .. } => 25,
            ServerMessage::AgentLifeChanged { .. } => 26,
            ServerMessage::LaunchMissile { .. } => 27,
            ServerMessage::AgentManaUpdated { .. } => 28,
            ServerMessage::PlayerSkills { .. } => 29,
            ServerMessage::SkillUpdated { .. } => 30,
            ServerMessage::ExperienceUpdated { .. } => 31,
            ServerMessage::SpellCast { .. } => 32,
            ServerMessage::SpellList { .. } => 33,
            ServerMessage::AgentSpeedUpdated { .. } => 34,
            ServerMessage::PlayerStatus { .. } => 35,
        }
    }

    fn every_server_message() -> Vec<ServerMessage> {
        let tiles: Box<[ItemStack; VIEWPORT_SIZE]> = Box::new(std::array::from_fn(|i| {
            let mut stack: ItemStack = Default::default();
            stack[0] = Some((ItemId(100 + i as u16), 1));
            stack
        }));

        let mut one_item: ItemStack = Default::default();
        one_item[0] = Some((ItemId(266), 1));

        vec![
            ServerMessage::Pong,
            ServerMessage::LoginError,
            ServerMessage::DescribePlayer {
                agent_id: AgentId(7),
                position: Position::new(100, 200, 7),
                facing: Facing::North,
                name: "Bob".to_string(),
                level: 9,
                life: Pool {
                    current: 30,
                    maximum: 40,
                },
                mana: Pool {
                    current: 50,
                    maximum: 60,
                },
                outfit: (OutfitId(128), OutfitColors::new(1, 2, 3, 4)),
                speed: 120,
                capacity: 340,
                inventory_head: Some(ItemId(1)),
                inventory_amulet: None,
                inventory_backpack: Some(ItemId(2)),
                inventory_chest: None,
                inventory_right_hand: Some(ItemId(3)),
                inventory_left_hand: None,
                inventory_legs: Some(ItemId(4)),
                inventory_feet: None,
                inventory_ring: Some(ItemId(5)),
                inventory_trinket: None,
            },
            ServerMessage::DescribeMap {
                tiles,
                center: Position::new(32097, 31103, 7),
                floor: 7,
            },
            ServerMessage::TileUpdated {
                position: Position::new(32097, 31103, 7),
                items: Box::new(one_item),
            },
            ServerMessage::PlayerWalkAck {
                position: Position::new(32098, 31103, 7),
                tiles: vec![(7u8, vec![Default::default(); 3].into_boxed_slice())],
            },
            ServerMessage::PlayerWalkAck {
                position: Position::new(32098, 31103, 7),
                tiles: vec![],
            },
            ServerMessage::PlayerPosition {
                position: Position::new(1, 2, 7),
            },
            ServerMessage::TextMessage {
                text: "You cannot do that".to_string(),
                message_type: TextMessageType::ActionDenied,
            },
            ServerMessage::TextMessage {
                text: "You see a sword".to_string(),
                message_type: TextMessageType::Look,
            },
            ServerMessage::OpenContainer {
                container_id: ContainerId(3),
                capacity: 8,
                has_parent: false,
                title: "bag".to_string(),
                items: vec![Some((ItemId(100), 1))].into_boxed_slice(),
            },
            ServerMessage::UpdateContainer {
                container_id: ContainerId(3),
                items: vec![Some((ItemId(100), 1))].into_boxed_slice(),
            },
            ServerMessage::ContainerClosed {
                container_id: ContainerId(3),
            },
            ServerMessage::PlayerWalkDenied,
            ServerMessage::IventorySlotUpdated {
                slot: InventorySlot::Backpack,
                item_id: Some(ItemId(2)),
            },
            ServerMessage::IventorySlotUpdated {
                slot: InventorySlot::Head,
                item_id: None,
            },
            ServerMessage::PlayerCapacityUpdated { cap: 400 },
            ServerMessage::AgentChangedDirection {
                agent_id: AgentId(9),
                facing: Facing::East,
            },
            ServerMessage::RemoveAgent {
                agent_id: AgentId(9),
            },
            ServerMessage::MoveAgent {
                agent_id: AgentId(9),
                direction: Direction::SouthWest,
                from: Position::new(10, 10, 7),
            },
            ServerMessage::SpawnAgent {
                agent_id: AgentId(9),
                outfit: (OutfitId(1), OutfitColors::new(1, 2, 3, 4)),
                position: Position::new(10, 10, 7),
                facing: Facing::South,
                name: "Rat".to_string(),
                life: 20,
                speed: 100,
            },
            ServerMessage::TeleportAgent {
                agent_id: AgentId(9),
                position: Position::new(11, 11, 7),
            },
            ServerMessage::ChatMessage {
                author: "Rizael".to_string(),
                message_type: ChatMessageType::Local,
                channel: ChannelId(0),
                position: Some(Position::new(300, 400, 7)),
                message: "hello".to_string(),
            },
            ServerMessage::ChatMessage {
                author: "Rizael".to_string(),
                message_type: ChatMessageType::Channel,
                channel: ChannelId(7),
                position: None,
                message: "hello".to_string(),
            },
            ServerMessage::ChannelList {
                channels: vec![
                    (ChannelId(1), "World Chat".to_string()),
                    (ChannelId(7), "Help".to_string()),
                ],
            },
            ServerMessage::PrivateChatOpened {
                name: "Rizael".to_string(),
            },
            ServerMessage::FloatingText {
                text: "-25".to_string(),
                position: Position::new(100, 200, 7),
                text_type: FloatingTextType::HitPoints,
                color: Some(Color(255, 0, 64)),
            },
            ServerMessage::FloatingText {
                text: "hi".to_string(),
                position: Position::new(100, 200, 7),
                text_type: FloatingTextType::CreatureSay,
                color: None,
            },
            ServerMessage::TargetLost { seq: 77 },
            ServerMessage::DamagedBy {
                agent_id: AgentId(9),
            },
            ServerMessage::ShowEffect {
                effect_id: EffectId(13),
                position: Position::new(100, 200, 7),
                delta: vec![(0, 0)],
            },
            ServerMessage::ShowEffect {
                effect_id: EffectId(13),
                position: Position::new(100, 200, 7),
                delta: vec![(0, -1), (0, -2)],
            },
            ServerMessage::AgentLifeChanged {
                agent_id: AgentId(9),
                current: 30,
                max: 40,
            },
            ServerMessage::LaunchMissile {
                from: Position::new(1, 1, 7),
                to: Position::new(2, 2, 7),
                missile_id: MissileId(5),
            },
            ServerMessage::AgentManaUpdated {
                agent_id: AgentId(9),
                current: 10,
                max: 20,
            },
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
            ServerMessage::SkillUpdated {
                skill: SkillType::Sword,
                progress: SkillProgress {
                    level: 12,
                    percent_bp: 4909,
                },
            },
            ServerMessage::ExperienceUpdated { experience: 4231 },
            ServerMessage::SpellCast {
                spell: SpellId(4),
                spell_cooldown_ms: 1000,
                group_cooldown_ms: 2000,
            },
            ServerMessage::SpellList {
                spells: vec![SpellListEntry {
                    id: SpellId(4),
                    name: "Ab".to_string(),
                    words: "cd".to_string(),
                    level: 12,
                    icon: 29,
                    aimable: true,
                    group: SpellGroup::Support,
                }],
            },
            ServerMessage::AgentSpeedUpdated {
                agent_id: AgentId(9),
                speed: 130,
            },
            ServerMessage::PlayerStatus { status: 0x0A },
        ]
    }

    #[test]
    fn every_server_message_variant_has_a_sample() {
        let indices: std::collections::BTreeSet<usize> = every_server_message()
            .iter()
            .map(server_message_variant_index)
            .collect();

        assert_eq!(indices, (0..SERVER_MESSAGE_VARIANTS).collect());
    }

    #[test]
    fn this_codec_decodes_everything_the_server_encodes() {
        for message in every_server_message() {
            let mut buf = BytesMut::new();
            ServerCodec {}
                .encode(message.clone(), &mut buf)
                .expect("encoding");

            let decoded = LoadtestCodec::default().decode(&mut buf).expect("decoding");

            assert_eq!(decoded, Some(message), "round trip");
            assert!(buf.is_empty(), "the frame left trailing bytes");
        }
    }

    #[test]
    fn bytes_read_counts_the_frame_including_its_length_prefix_but_not_a_partial_one() {
        let mut buf = BytesMut::new();
        ServerCodec {}
            .encode(ServerMessage::Pong, &mut buf)
            .unwrap();
        let full_len = buf.len() as u64;
        let mut partial = buf.clone().split_to(1);

        let mut codec = LoadtestCodec::default();
        assert_eq!(codec.decode(&mut partial).unwrap(), None);
        assert_eq!(
            codec.bytes_read(),
            0,
            "an incomplete frame must not be counted yet"
        );

        codec.decode(&mut buf).unwrap();
        assert_eq!(codec.bytes_read(), full_len);
    }

    #[test]
    fn a_partial_frame_decodes_to_none() {
        let mut buf = BytesMut::new();
        ServerCodec {}
            .encode(ServerMessage::Pong, &mut buf)
            .unwrap();
        let mut partial = buf.split_to(1);

        assert_eq!(LoadtestCodec::default().decode(&mut partial).unwrap(), None);
    }

    /// The server's own decoder uses unchecked reads throughout, so a truncated frame
    /// anywhere in any arm has to return rather than panic. Cutting a sample exactly on
    /// a `ShowEffect` delta-pair boundary is a legitimate `Ok`, so this only checks that
    /// `decode` returns for every length, not which way it returns.
    #[test]
    fn every_truncation_of_every_server_message_returns_without_panicking() {
        for message in every_server_message() {
            let mut full = BytesMut::new();
            ServerCodec {}.encode(message.clone(), &mut full).unwrap();
            let full_payload_len = full.len() - 2;

            for len in 0..=full_payload_len {
                let mut frame = BytesMut::new();
                frame.put_u16_le(len as u16);
                frame.extend_from_slice(&full[2..2 + len]);

                let _ = LoadtestCodec::default().decode(&mut frame);
            }
        }
    }

    #[test]
    fn a_trailing_byte_inside_the_payload_is_rejected() {
        let mut buf = BytesMut::new();
        ServerCodec {}
            .encode(ServerMessage::Pong, &mut buf)
            .unwrap();
        let declared_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        buf[0..2].copy_from_slice(&((declared_len + 1) as u16).to_le_bytes());
        buf.put_u8(0xFF);

        assert!(LoadtestCodec::default().decode(&mut buf).is_err());
    }
}
