use tracing::error;

use crate::{
    entities::{
        agent::{AgentKey, Facing},
        items::{Item, ItemId},
        position::Position,
    },
    game::{TickCtx, events::BroadcastMessage},
    persistence::items::ITEM_CONFIGS,
};

pub fn parse_command(ctx: &mut TickCtx, command: &str, agent_key: AgentKey) -> bool {
    if !ctx
        .map
        .get_player(agent_key)
        .map(|p| p.admin())
        .unwrap_or(false)
    {
        return false;
    }

    let parts: Vec<&str> = command.split(" ").collect();
    match parts.first() {
        Some(&"/create") => {
            let Some(item_id) = parts.get(1).and_then(|p| p.parse().ok()).map(ItemId) else {
                return true;
            };
            let amount: u8 = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(1);
            create_item(ctx, item_id, amount, agent_key);
            true
        }
        Some(&"/tp") => {
            let Some(current_pos) = ctx.map.agent_position(agent_key).cloned() else {
                return true;
            };
            let (Some(x), Some(y), Some(z)) = (
                parts.get(1).and_then(|x| x.parse::<u16>().ok()),
                parts.get(2).and_then(|y| y.parse::<u16>().ok()),
                parts.get(3).and_then(|z| z.parse::<u8>().ok()),
            ) else {
                return true;
            };

            let pos = Position::new(x, y, z);
            if ctx.map.can_move(&pos, agent_key) && ctx.map.move_agent(agent_key, &pos).is_ok() {
                ctx.events.push(BroadcastMessage::AgentTeleported {
                    agent_key,
                    from_position: current_pos,
                    to_position: pos,
                });
            }

            true
        }
        Some(cmd) => {
            error!("Invalid command: {}", cmd);
            false
        }
        None => false,
    }
}

pub fn create_item(ctx: &mut TickCtx, id: ItemId, amount: u8, agent_key: AgentKey) {
    let Some(config) = ITEM_CONFIGS.get(&id) else {
        return;
    };
    let item = Item::new(config.clone(), amount);
    let Some(facing) = ctx.map.get_agent(agent_key).map(|a| a.facing()) else {
        return;
    };
    let Some(pos) = ctx.map.agent_position(agent_key).cloned() else {
        return;
    };
    let at_pos = match facing {
        Facing::North => Position::new(pos.x, pos.y.saturating_sub(1), pos.z),
        Facing::East => Position::new(pos.x.saturating_add(1), pos.y, pos.z),
        Facing::South => Position::new(pos.x, pos.y.saturating_add(1), pos.z),
        Facing::West => Position::new(pos.x.saturating_sub(1), pos.y, pos.z),
    };
    let _ = ctx.map.place_item(&at_pos, None, None, item);
}
