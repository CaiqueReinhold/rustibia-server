use crate::entities::agent::AgentKey;
use crate::game::TickCtx;
use crate::game::events::BroadcastMessage;
use crate::game::map_query::can_target;

pub fn set_target(ctx: &mut TickCtx, agent: AgentKey, target: Option<AgentKey>, seq: u32) {
    if ctx.map.get_agent(agent).is_none() {
        return;
    }

    let requested_a_target = target.is_some();
    let accepted = match target {
        Some(t) if t != agent && ctx.map.get_agent(t).is_some() => Some(t),
        _ => None,
    };

    ctx.map
        .agent_mut(agent)
        .expect("agent was just found by get_agent above")
        .set_target(accepted, if accepted.is_some() { seq } else { 0 });

    if requested_a_target && accepted.is_none() {
        ctx.events.push(BroadcastMessage::AgentLostTarget {
            agent_key: agent,
            seq,
        });
    }
}

/// Clears `agent`'s target and announces the loss, stamped with the seq that set
/// it.
pub fn lose_target(ctx: &mut TickCtx, agent: AgentKey) {
    let Some(actor) = ctx.map.get_agent(agent) else {
        return;
    };
    if actor.target().is_none() {
        return;
    }
    let seq = actor.target_seq();

    ctx.map
        .agent_mut(agent)
        .expect("agent was just found by get_agent above")
        .set_target(None, 0);

    ctx.events.push(BroadcastMessage::AgentLostTarget {
        agent_key: agent,
        seq,
    });
}

/// Drops `agent`'s target if it is gone or no longer targetable. Returns whether the target was
/// dropped, so the caller can skip an attack it no longer has a target for.
///
/// An absent attacker drops nothing rather than clearing: it died earlier in the
/// same pass, and a dead agent's loss has no session to reach.
pub fn drop_unreachable_target(ctx: &mut TickCtx, agent: AgentKey) -> bool {
    let Some(actor) = ctx.map.get_agent(agent) else {
        return false;
    };
    let Some(target) = actor.target() else {
        return false;
    };
    let Some(from) = ctx.map.agent_position(agent) else {
        return false;
    };

    let reachable = match (ctx.map.get_agent(target), ctx.map.agent_position(target)) {
        (Some(_), Some(to)) => can_target(from, to),
        _ => false,
    };
    if reachable {
        return false;
    }

    lose_target(ctx, agent);
    true
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

    fn seat(map: &mut GameMap, at: &Position, id: u32) -> AgentKey {
        map.insert_tile(at.clone(), MapTile::new());
        map.insert_agent(Agent::from_player(a_test_snapshot(id, 1)), at)
            .unwrap()
    }

    fn map_with_two_players() -> (GameMap, AgentKey, AgentKey) {
        let mut map = GameMap::new();
        let a = Position::new(10, 10, 7);
        let b = Position::new(11, 10, 7);
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());
        let attacker = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &a)
            .unwrap();
        let victim = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &b)
            .unwrap();
        (map, attacker, victim)
    }

    fn target_of(map: &GameMap, key: AgentKey) -> Option<AgentKey> {
        map.get_agent(key).unwrap().target()
    }

    #[test]
    fn sets_a_valid_target_and_says_nothing() {
        let mut h = TestHarness::new();
        let (map, attacker, victim) = map_with_two_players();

        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), Some(victim));
        assert_eq!(map.get_agent(attacker).unwrap().target_seq(), 5);
        assert!(
            msgs.is_empty(),
            "the client applied it optimistically; there is nothing to tell it"
        );
    }

    #[test]
    fn clears_on_none_and_says_nothing() {
        let mut h = TestHarness::new();
        let (map, attacker, victim) = map_with_two_players();
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);

        set_target(&mut h.ctx(&mut map), attacker, None, 6);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), None);
        assert!(msgs.is_empty());
    }

    /// A rejection must reach the client, or it is left drawing a square the
    /// server does not hold. It carries the seq just rejected, because that is the
    /// one the client applied.
    #[test]
    fn rejecting_a_missing_agent_announces_the_loss_with_the_new_seq() {
        let mut h = TestHarness::new();
        let (map, attacker, victim) = map_with_two_players();
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);
        map.inner_mut().remove_agent(victim);

        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 6);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), None);
        assert!(matches!(
            msgs.as_slice(),
            [BroadcastMessage::AgentLostTarget { agent_key, seq }]
                if *agent_key == attacker && *seq == 6
        ));
    }

    #[test]
    fn rejects_self_targeting() {
        let mut h = TestHarness::new();
        let (map, attacker, _) = map_with_two_players();

        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(attacker), 7);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), None);
        assert!(matches!(
            msgs.as_slice(),
            [BroadcastMessage::AgentLostTarget { seq: 7, .. }]
        ));
    }

    #[test]
    fn an_absent_actor_changes_nothing() {
        let mut h = TestHarness::new();
        let (mut map, attacker, victim) = map_with_two_players();
        map.remove_agent(attacker);

        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 1);
        assert!(h.events.is_empty());
    }

    #[test]
    fn a_reachable_target_is_kept() {
        let mut h = TestHarness::new();
        let (map, attacker, victim) = map_with_two_players();
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);

        drop_unreachable_target(&mut h.ctx(&mut map), attacker);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), Some(victim));
        assert!(msgs.is_empty());
    }

    /// Out of weapon range is not out of reach: the attacker walks closer.
    #[test]
    fn a_target_out_of_weapon_range_but_in_view_is_kept() {
        let mut h = TestHarness::new();
        let mut map = GameMap::new();
        let attacker = seat(&mut map, &Position::new(100, 100, 7), 1);
        let victim = seat(&mut map, &Position::new(105, 105, 7), 2);
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);

        drop_unreachable_target(&mut h.ctx(&mut map), attacker);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), Some(victim));
        assert!(msgs.is_empty());
    }

    #[test]
    fn a_target_outside_the_viewport_is_dropped_with_its_seq() {
        let mut h = TestHarness::new();
        let mut map = GameMap::new();
        let attacker = seat(&mut map, &Position::new(100, 100, 7), 1);
        let victim = seat(&mut map, &Position::new(110, 100, 7), 2);
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 9);

        drop_unreachable_target(&mut h.ctx(&mut map), attacker);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), None);
        assert!(matches!(
            msgs.as_slice(),
            [BroadcastMessage::AgentLostTarget { agent_key, seq }]
                if *agent_key == attacker && *seq == 9
        ));
    }

    #[test]
    fn a_target_just_inside_the_viewport_is_kept() {
        let mut h = TestHarness::new();
        let mut map = GameMap::new();
        let attacker = seat(&mut map, &Position::new(100, 100, 7), 1);
        let victim = seat(&mut map, &Position::new(109, 107, 7), 2);
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);

        assert!(!drop_unreachable_target(&mut h.ctx(&mut map), attacker));
        assert_eq!(target_of(&map, attacker), Some(victim));
    }

    #[test]
    fn a_target_one_floor_up_is_dropped() {
        let mut h = TestHarness::new();
        let mut map = GameMap::new();
        let attacker = seat(&mut map, &Position::new(100, 100, 7), 1);
        let victim = seat(&mut map, &Position::new(101, 100, 6), 2);
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);

        drop_unreachable_target(&mut h.ctx(&mut map), attacker);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), None);
        assert_eq!(msgs.len(), 1);
    }

    /// The dangling `AgentKey` a logout or a reaped corpse leaves behind. No
    /// session has to notice for it to be cleared.
    #[test]
    fn a_target_that_left_the_map_is_dropped() {
        let mut h = TestHarness::new();
        let (map, attacker, victim) = map_with_two_players();
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);
        map.inner_mut().remove_agent(victim);

        drop_unreachable_target(&mut h.ctx(&mut map), attacker);
        let msgs = &h.events;

        assert_eq!(target_of(&map, attacker), None);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn an_agent_with_no_target_produces_nothing() {
        let mut h = TestHarness::new();
        let (map, attacker, _) = map_with_two_players();

        let mut map = WorldMap::new(map);
        assert!(!drop_unreachable_target(&mut h.ctx(&mut map), attacker));
    }

    /// It died earlier in the same pass. Clearing a dead agent's target must not
    /// emit an event for a session to translate.
    #[test]
    fn an_absent_attacker_produces_nothing() {
        let mut h = TestHarness::new();
        let (map, attacker, victim) = map_with_two_players();
        let mut map = WorldMap::new(map);
        set_target(&mut h.ctx(&mut map), attacker, Some(victim), 5);
        map.inner_mut().remove_agent(attacker);

        assert!(!drop_unreachable_target(&mut h.ctx(&mut map), attacker));
    }

    #[test]
    fn lose_target_on_an_agent_with_no_target_says_nothing() {
        let mut h = TestHarness::new();
        let (map, attacker, _) = map_with_two_players();

        let mut map = WorldMap::new(map);
        lose_target(&mut h.ctx(&mut map), attacker);
        assert!(h.events.is_empty());
    }
}
