use crate::entities::agent::AgentKey;
use crate::game::TickCtx;
use crate::game::admin::parse_command;
use crate::game::events::BroadcastMessage;

pub fn say(ctx: &mut TickCtx, agent_key: AgentKey, message: String) {
    let Some(position) = ctx.map.agent_position(agent_key).cloned() else {
        return;
    };

    if parse_command(ctx, &message, agent_key) {
        return;
    }

    ctx.events.push(BroadcastMessage::AgentSaid {
        agent_key,
        position,
        message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::position::Position;
    use crate::entities::world_map::WorldMap;
    use crate::game::TestHarness;
    use crate::persistence::test_fixtures::a_test_snapshot;

    #[test]
    fn a_live_agent_produces_one_event() {
        let pos = Position::new(100, 100, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();

        let mut h = TestHarness::new();
        let mut map = WorldMap::new(map);
        say(&mut h.ctx(&mut map), key, "hello".to_owned());

        let events = h.events;
        assert_eq!(events.len(), 1);
        match &events[0] {
            BroadcastMessage::AgentSaid {
                agent_key,
                position,
                message,
            } => {
                assert_eq!(*agent_key, key);
                assert_eq!(*position, pos);
                assert_eq!(message, "hello");
            }
            other => panic!("expected AgentSaid, got {other:?}"),
        }
    }

    /// A player can log out between sending and the tick that processes it.
    #[test]
    fn an_agent_no_longer_on_the_map_produces_nothing() {
        let mut h = TestHarness::new();
        say(
            &mut h.ctx(&mut WorldMap::new(GameMap::new())),
            AgentKey::default(),
            "hello".to_owned(),
        );
        assert!(h.events.is_empty());
    }
}
