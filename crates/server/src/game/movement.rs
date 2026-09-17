use tracing::error;

use crate::entities::{
    agent::{AgentKey, Facing},
    items::FloorChangeDirection,
    position::{Direction, Position},
};

use super::TickCtx;
use super::events::BroadcastMessage;

pub fn walk(ctx: &mut TickCtx, direction: Direction, agent_key: AgentKey) {
    let Some(agent) = ctx.map.get_agent(agent_key) else {
        error!("agent {agent_key:?} not spawned");
        return;
    };
    let next_walk_tick = agent.next_walk_tick;
    let Some(current_pos) = ctx.map.agent_position(agent_key).cloned() else {
        error!("agent {agent_key:?} position not found");
        return;
    };

    if next_walk_tick > ctx.tick {
        ctx.events
            .push(BroadcastMessage::AgentWalkDenied { agent_key });
        return;
    }

    let new_pos = current_pos.clone() + direction;
    if !ctx.map.can_move(&new_pos, agent_key) {
        let facing = agent.facing();
        if direction_to_facing(&direction) != facing {
            let agent = ctx.map.get_agent_mut(agent_key).unwrap();
            agent.set_facing(direction_to_facing(&direction));
            ctx.events.push(BroadcastMessage::AgentChangedDirection {
                agent_key,
                facing,
                position: current_pos,
            });
        }
        ctx.events
            .push(BroadcastMessage::AgentWalkDenied { agent_key });
        return;
    }

    let Some(tile_friction) = ctx.map.tile_friction(&new_pos) else {
        ctx.events
            .push(BroadcastMessage::AgentWalkDenied { agent_key });
        return;
    };

    let walk_ticks = ctx
        .map
        .get_agent(agent_key)
        .unwrap()
        .calculate_walk_ticks(tile_friction, direction.is_diagonal());

    let agent = ctx.map.get_agent_mut(agent_key).unwrap();
    agent.next_walk_tick = ctx.tick + walk_ticks;
    agent.set_facing(direction_to_facing(&direction));
    if let Err(e) = ctx.map.move_agent(agent_key, &new_pos) {
        error!("agent {agent_key:?} could not walk to {new_pos}: {e:?}");
        return;
    }
    let floor_change = ctx.map.get_floor_change(&new_pos);

    ctx.events.push(BroadcastMessage::AgentMoved {
        agent_key,
        direction,
        from_position: current_pos,
        to_position: new_pos.clone(),
    });

    if let Some(floor_change) = floor_change {
        let position = match floor_change {
            FloorChangeDirection::Up => Position::new(new_pos.x, new_pos.y, new_pos.z - 1),
            FloorChangeDirection::Down => {
                if let Some(downstairs_change) =
                    ctx.map
                        .get_floor_change(&Position::new(new_pos.x, new_pos.y, new_pos.z + 1))
                {
                    let (x, y) = match downstairs_change {
                        FloorChangeDirection::Up | FloorChangeDirection::Down => {
                            (new_pos.x, new_pos.y)
                        }
                        FloorChangeDirection::North => (new_pos.x, new_pos.y + 1),
                        FloorChangeDirection::East => (new_pos.x - 1, new_pos.y),
                        FloorChangeDirection::South => (new_pos.x, new_pos.y - 1),
                        FloorChangeDirection::West => (new_pos.x + 1, new_pos.y),
                    };
                    Position::new(x, y, new_pos.z + 1)
                } else {
                    Position::new(new_pos.x, new_pos.y, new_pos.z + 1)
                }
            }
            FloorChangeDirection::North => Position::new(new_pos.x, new_pos.y - 1, new_pos.z - 1),
            FloorChangeDirection::East => Position::new(new_pos.x + 1, new_pos.y, new_pos.z - 1),
            FloorChangeDirection::South => Position::new(new_pos.x, new_pos.y + 1, new_pos.z - 1),
            FloorChangeDirection::West => Position::new(new_pos.x - 1, new_pos.y, new_pos.z - 1),
        };
        if let Err(e) = ctx.map.move_agent(agent_key, &position) {
            error!("agent {agent_key:?} could not take the floor change to {position}: {e:?}");
            return;
        }
        ctx.events.push(BroadcastMessage::AgentTeleported {
            agent_key,
            from_position: new_pos,
            to_position: position,
        });
    }
}

fn direction_to_facing(direction: &Direction) -> Facing {
    match direction {
        Direction::North => Facing::North,
        Direction::East => Facing::East,
        Direction::South => Facing::South,
        Direction::West => Facing::West,
        Direction::NorthEast => Facing::East,
        Direction::NorthWest => Facing::West,
        Direction::SouthEast => Facing::East,
        Direction::SouthWest => Facing::West,
    }
}

pub fn change_direction(ctx: &mut TickCtx, agent_key: AgentKey, facing: Facing) {
    let current_facing = ctx.map.get_agent(agent_key).map(|agent| agent.facing());
    if let Some(current_facing) = current_facing
        && facing != current_facing
    {
        ctx.map.get_agent_mut(agent_key).unwrap().set_facing(facing);
        let position = ctx
            .map
            .agent_position(agent_key)
            .cloned()
            .unwrap_or_default();
        ctx.events.push(BroadcastMessage::AgentChangedDirection {
            agent_key,
            facing,
            position,
        });
    }
}
