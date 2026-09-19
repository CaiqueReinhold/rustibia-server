use tracing::error;

use crate::{
    entities::{
        agent::{AgentKey, Facing},
        items::{Item, ItemId},
        position::Position,
    },
    game::TickCtx,
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
