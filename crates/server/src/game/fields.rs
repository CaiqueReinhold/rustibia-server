use std::sync::Arc;

use crate::{
    entities::{
        agent::AgentKey,
        items::{Item, ItemConfig, ItemFlag, ItemGuid},
        map::GameMap,
        position::{ItemPlacement, Position},
    },
    game::{TickCtx, conditions::apply_condition, item_action::check_decay},
};

pub fn can_hold_field(map: &GameMap, pos: &Position) -> bool {
    let Ok(items) = map.iter_items(pos) else {
        return false;
    };
    let mut has_ground = false;
    for item in items {
        if item.config.has_flag(ItemFlag::Unpass) || item.config.has_flag(ItemFlag::Unreplaceable) {
            return false;
        }
        has_ground |= item.config.has_flag(ItemFlag::Ground);
    }
    has_ground
}

pub fn create_fields(
    ctx: &mut TickCtx,
    owner: AgentKey,
    field: &Arc<ItemConfig>,
    area: &[Position],
) {
    for pos in area {
        if can_hold_field(ctx.map, pos) {
            replace_field(ctx, owner, field, pos);
        }
    }
}

fn replace_field(ctx: &mut TickCtx, owner: AgentKey, field: &Arc<ItemConfig>, pos: &Position) {
    let Ok(items) = ctx.map.iter_items(pos) else {
        return;
    };
    let replaced: Vec<ItemGuid> = items
        .filter(|item| item.config.attr_field().is_some())
        .map(|item| item.guid)
        .collect();
    for guid in &replaced {
        ctx.map.remove_item_from_tile(pos, guid, 1);
    }

    let mut item = Item::new(field.clone(), 1);
    item.owner = Some(owner);
    let tick = ctx.tick;
    let Ok(placed) = ctx.map.place_item(pos, None, None, item) else {
        return;
    };
    check_decay(ctx.scheduled, placed, ItemPlacement::Map(pos.clone()), tick);

    let Some(spec) = field.attr_field().cloned() else {
        return;
    };
    let standing: Vec<AgentKey> = ctx
        .map
        .iter_agents_at(pos)
        .map(|agents| agents.copied().collect())
        .unwrap_or_default();
    for agent in standing {
        apply_condition(ctx, agent, &spec, Some(owner));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::Bounds;
    use crate::entities::agent::Agent;
    use crate::entities::combat::CombatElement;
    use crate::entities::conditions::{ConditionSpec, SpecSchedule};
    use crate::entities::items::{ItemAttribute, ItemId};
    use crate::entities::map::MapTile;
    use crate::entities::world_map::WorldMap;
    use crate::game::events::BroadcastMessage;
    use crate::game::{TestHarness, Tick, TickDelta};
    use crate::persistence::test_fixtures::{a_test_creature, a_test_snapshot};

    fn a_config(id: u16, flags: Vec<ItemFlag>, attributes: Vec<ItemAttribute>) -> Arc<ItemConfig> {
        Arc::new(ItemConfig::new(
            ItemId(id),
            "thing".to_string(),
            None,
            None,
            flags,
            attributes,
        ))
    }

    fn a_field_config(id: u16, flags: Vec<ItemFlag>) -> Arc<ItemConfig> {
        a_config(
            id,
            flags,
            vec![
                ItemAttribute::Field(Arc::new(ConditionSpec {
                    element: CombatElement::Fire,
                    damage: Bounds { min: 20, max: 20 },
                    interval: TickDelta(200),
                    schedule: SpecSchedule::Flat { count: 7 },
                    delayed: false,
                })),
                ItemAttribute::Decay {
                    duration: TickDelta(100),
                    decay_to: ItemId(0),
                },
            ],
        )
    }

    fn a_tile(extra: Vec<Arc<ItemConfig>>) -> MapTile {
        let mut tile = MapTile::new();
        tile.push_item(Item::new(
            a_config(1, vec![ItemFlag::Ground], Vec::new()),
            1,
        ));
        for config in extra {
            tile.push_item(Item::new(config, 1));
        }
        tile
    }

    const AT: Position = Position { x: 10, y: 10, z: 7 };

    /// A caster standing off to the side, and a tile at `AT` holding `extra` over its ground.
    fn a_map(extra: Vec<Arc<ItemConfig>>) -> (GameMap, AgentKey) {
        let aside = Position::new(20, 20, 7);
        let mut map = GameMap::new();
        map.insert_tile(AT, a_tile(extra));
        map.insert_tile(aside.clone(), MapTile::new());
        let caster = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &aside)
            .unwrap();
        (map, caster)
    }

    fn ids_at(map: &WorldMap, pos: &Position) -> Vec<ItemId> {
        map.iter_items(pos)
            .unwrap()
            .map(|item| item.id())
            .collect()
    }

    #[test]
    fn a_field_lands_on_top_of_everything_with_its_owner_and_its_decay() {
        let (map, caster) = a_map(vec![a_config(3, Vec::new(), Vec::new())]);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        create_fields(
            &mut h.ctx(&mut map),
            caster,
            &a_field_config(5, Vec::new()),
            &[AT],
        );

        assert_eq!(ids_at(&map, &AT), vec![ItemId(1), ItemId(3), ItemId(5)]);
        assert_eq!(map.get_item_at(&AT, 2).unwrap().owner, Some(caster));
        assert!(h.scheduled.iter().any(|s| s.at_tick == Tick(100)));
    }

    #[test]
    fn a_new_field_replaces_a_replaceable_one_and_stays_on_top() {
        let (map, caster) = a_map(vec![
            a_field_config(4, Vec::new()),
            a_config(3, Vec::new(), Vec::new()),
        ]);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        create_fields(
            &mut h.ctx(&mut map),
            caster,
            &a_field_config(5, Vec::new()),
            &[AT],
        );

        assert_eq!(ids_at(&map, &AT), vec![ItemId(1), ItemId(3), ItemId(5)]);
    }

    #[test]
    fn an_unreplaceable_field_or_a_wall_keeps_the_field_out() {
        for blocker in [
            a_field_config(4, vec![ItemFlag::Unreplaceable]),
            a_config(4, vec![ItemFlag::Unpass], Vec::new()),
        ] {
            let (map, caster) = a_map(vec![blocker]);
            let mut map = WorldMap::new(map);
            let mut h = TestHarness::seeded(1);

            assert!(!can_hold_field(&map, &AT));
            create_fields(
                &mut h.ctx(&mut map),
                caster,
                &a_field_config(5, Vec::new()),
                &[AT],
            );

            assert_eq!(ids_at(&map, &AT), vec![ItemId(1), ItemId(4)]);
        }
    }

    #[test]
    fn a_tile_without_ground_cannot_hold_a_field() {
        let mut map = GameMap::new();
        map.insert_tile(AT, MapTile::new());

        assert!(!can_hold_field(&map, &AT));
    }

    #[test]
    fn someone_already_standing_there_is_hit_at_once_and_the_caster_is_credited() {
        let (mut map, caster) = a_map(Vec::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &AT)
            .unwrap();
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        create_fields(
            &mut h.ctx(&mut map),
            caster,
            &a_field_config(5, Vec::new()),
            &[AT],
        );

        assert_eq!(map.get_agent(rat).unwrap().life().current, 80);
        assert!(h.events.iter().any(|e| matches!(
            e,
            BroadcastMessage::DamageTaken { source: Some(s), target, .. }
                if *s == caster && *target == rat
        )));
    }
}
