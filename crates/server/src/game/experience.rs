use crate::entities::agent::AgentKey;
use crate::entities::skills::SkillType;
use crate::game::TickCtx;
use crate::game::skills::tick_skill;

/// Splits the victim's experience across everyone who damaged it, in proportion to the
/// damage each dealt.
pub fn award(ctx: &mut TickCtx, victim: AgentKey) {
    let Some(agent) = ctx.map.get_agent(victim) else {
        return;
    };
    let Some(kind) = agent.get_creature_kind() else {
        return;
    };
    let shares = agent.participation().shares(kind.experience);

    for (key, share) in shares {
        tick_skill(ctx, key, SkillType::Level, share * 1000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::position::Position;
    use crate::game::TestHarness;
    use crate::game::events::BroadcastMessage;
    use crate::persistence::test_fixtures::{
        a_test_creature, a_test_creature_worth, a_test_snapshot,
    };

    fn a_victim(experience: u32) -> (GameMap, AgentKey) {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature_worth("Rat", 100, (1, 2), experience), &pos)
            .unwrap();
        (map, rat)
    }

    fn add_player(map: &mut GameMap, id: u32, position: Position) -> AgentKey {
        map.insert_tile(position.clone(), MapTile::new());
        map.insert_agent(
            Agent::from_player(a_test_snapshot(id, id as i32)),
            &position,
        )
        .unwrap()
    }

    fn add_creature(map: &mut GameMap, name: &str, position: Position) -> AgentKey {
        map.insert_tile(position.clone(), MapTile::new());
        map.insert_agent(a_test_creature(name, 100, (1, 2)), &position)
            .unwrap()
    }

    fn hit(map: &mut GameMap, victim: AgentKey, attacker: AgentKey, damage: u32) {
        map.get_agent_mut(victim)
            .unwrap()
            .record_damage(attacker, damage);
    }

    fn level(map: &GameMap, key: AgentKey) -> (u16, u64) {
        let value = map
            .get_player(key)
            .unwrap()
            .skills()
            .get(&SkillType::Level)
            .unwrap();
        (value.value, value.current_ticks)
    }

    fn upgraded(msgs: &[BroadcastMessage], key: AgentKey) -> bool {
        msgs.iter().any(|m| {
            matches!(
                m,
                BroadcastMessage::SkillUpgraded { agent_key, skill_type: SkillType::Level, .. }
                    if *agent_key == key
            )
        })
    }

    fn progressed(msgs: &[BroadcastMessage], key: AgentKey) -> bool {
        msgs.iter().any(|m| {
            matches!(
                m,
                BroadcastMessage::SkillProgressUpdated { agent_key, skill_type: SkillType::Level, .. }
                    if *agent_key == key
            )
        })
    }

    fn awarded(msgs: &[BroadcastMessage], key: AgentKey) -> Option<u64> {
        msgs.iter().find_map(|m| match m {
            BroadcastMessage::SkillUpgraded {
                agent_key,
                skill_type: SkillType::Level,
                amount,
                ..
            } if *agent_key == key => Some(*amount),
            _ => None,
        })
    }

    /// The level-up branch used to drop the experience on the floor, and the session
    /// floated a `0` over the player that had just levelled.
    #[test]
    fn a_kill_that_levels_still_reports_the_experience_it_awarded() {
        let (mut map, rat) = a_victim(100);
        let hunter = add_player(&mut map, 1, Position::new(11, 10, 7));
        hit(&mut map, rat, hunter, 100);
        let mut h = TestHarness::new();

        award(&mut h.ctx(&mut map), rat);

        assert!(upgraded(&h.events, hunter));
        assert_eq!(awarded(&h.events, hunter), Some(100));
    }

    /// `a_test_snapshot` starts at level 1 with no progress, and the step into level 2
    /// costs exactly 100 experience.
    #[test]
    fn a_solo_killer_takes_the_whole_pool_and_levels() {
        let (mut map, rat) = a_victim(100);
        let hunter = add_player(&mut map, 1, Position::new(11, 10, 7));
        hit(&mut map, rat, hunter, 100);
        let mut h = TestHarness::new();

        award(&mut h.ctx(&mut map), rat);

        assert_eq!(level(&map, hunter), (2, 0));
        assert!(upgraded(&h.events, hunter));
    }

    #[test]
    fn a_share_short_of_a_level_reports_progress_instead() {
        let (mut map, rat) = a_victim(50);
        let hunter = add_player(&mut map, 1, Position::new(11, 10, 7));
        hit(&mut map, rat, hunter, 100);
        let mut h = TestHarness::new();

        award(&mut h.ctx(&mut map), rat);

        assert_eq!(level(&map, hunter), (1, 50));
        assert!(progressed(&h.events, hunter));
        assert!(!upgraded(&h.events, hunter));
    }

    #[test]
    fn two_players_split_the_pool_by_damage() {
        let (mut map, rat) = a_victim(100);
        let first = add_player(&mut map, 1, Position::new(11, 10, 7));
        let second = add_player(&mut map, 2, Position::new(12, 10, 7));
        hit(&mut map, rat, first, 40);
        hit(&mut map, rat, second, 60);
        let mut h = TestHarness::new();

        award(&mut h.ctx(&mut map), rat);

        assert_eq!(level(&map, first), (1, 40));
        assert_eq!(level(&map, second), (1, 60));
    }

    /// A creature contributor earns nothing, but its damage stays in the denominator,
    /// so the rest of the pool is simply lost.
    #[test]
    fn a_creature_contributor_dilutes_the_players_share() {
        let (mut map, rat) = a_victim(100);
        let hunter = add_player(&mut map, 1, Position::new(11, 10, 7));
        let wolf = add_creature(&mut map, "Wolf", Position::new(12, 10, 7));
        hit(&mut map, rat, hunter, 50);
        hit(&mut map, rat, wolf, 50);
        let mut h = TestHarness::new();

        award(&mut h.ctx(&mut map), rat);

        assert_eq!(level(&map, hunter), (1, 50));
    }

    #[test]
    fn a_departed_attacker_is_skipped_but_still_dilutes() {
        let (mut map, rat) = a_victim(100);
        let hunter = add_player(&mut map, 1, Position::new(11, 10, 7));
        let quitter = add_player(&mut map, 2, Position::new(12, 10, 7));
        hit(&mut map, rat, hunter, 50);
        hit(&mut map, rat, quitter, 50);
        map.remove_agent(quitter);
        let mut h = TestHarness::new();

        award(&mut h.ctx(&mut map), rat);

        assert_eq!(level(&map, hunter), (1, 50));
        assert_eq!(h.events.len(), 1);
    }

    #[test]
    fn a_creature_worth_nothing_emits_nothing() {
        let (mut map, rat) = a_victim(0);
        let hunter = add_player(&mut map, 1, Position::new(11, 10, 7));
        hit(&mut map, rat, hunter, 100);
        let mut h = TestHarness::new();

        award(&mut h.ctx(&mut map), rat);

        assert_eq!(level(&map, hunter), (1, 0));
        assert!(h.events.is_empty());
    }

    #[test]
    fn an_untouched_creature_emits_nothing() {
        let (mut map, rat) = a_victim(100);
        let mut h = TestHarness::new();

        award(&mut h.ctx(&mut map), rat);

        assert!(h.events.is_empty());
    }
}
