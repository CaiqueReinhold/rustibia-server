use crate::{
    entities::{
        agent::AgentKey,
        items::{FluidType, Item, ItemFlag, ItemRef},
        map::GameMap,
        position::{ItemPlacement, Position},
    },
    game::{
        TickCtx,
        config::GAME_CONFIG,
        events::BroadcastMessage,
        item_action::{ItemActionError, check_decay},
        item_movement::{insert_item_at, remove_item_at},
        item_multi_action::UseTarget,
        map_query::find_item,
    },
    persistence::items::ITEM_CONFIGS,
};

const EMPTY: &str = "It is empty.";
const NOT_POSSIBLE: &str = "Not possible.";
const TOO_FAR: &str = "Too far away.";

enum Aim {
    Drink,
    Pour(Position),
    Fill(FluidType),
    Transfer(ItemRef, FluidType),
}

pub(super) fn use_fluid(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    source: &ItemRef,
    target: &UseTarget,
) -> Result<(), ItemActionError> {
    let held = find_item(ctx.map, &source.placement, &source.guid)
        .and_then(|item| item.fluid)
        .ok_or(ItemActionError::InvalidState)?;
    let aim = match aim(ctx.map, agent_key, source, target) {
        Ok(aim) => aim,
        Err(message) => return refuse(ctx, agent_key, message),
    };
    match aim {
        Aim::Fill(fluid) => refill(ctx, source, fluid),
        Aim::Transfer(other, theirs) => match (held, theirs) {
            (FluidType::None, FluidType::None) => refuse(ctx, agent_key, EMPTY),
            (_, FluidType::None) => {
                refill(ctx, &other, held)?;
                refill(ctx, source, FluidType::None)
            }
            (FluidType::None, _) => {
                refill(ctx, source, theirs)?;
                refill(ctx, &other, FluidType::None)
            }
            _ => refuse(ctx, agent_key, NOT_POSSIBLE),
        },
        _ if held == FluidType::None => refuse(ctx, agent_key, EMPTY),
        Aim::Drink => drink(ctx, agent_key, source, held),
        Aim::Pour(at) => pour(ctx, source, held, &at),
    }
}

fn aim(
    map: &GameMap,
    agent_key: AgentKey,
    source: &ItemRef,
    target: &UseTarget,
) -> Result<Aim, &'static str> {
    let at = map.agent_position(agent_key).ok_or(NOT_POSSIBLE)?;
    if let Some(agent) = target.agent {
        if agent == agent_key {
            return Ok(Aim::Drink);
        }
        let there = map.agent_position(agent).ok_or(NOT_POSSIBLE)?;
        return pour_at(map, at, there);
    }
    if let Some(item_ref) = target.item.as_ref().filter(|item| item.guid != source.guid)
        && let Some(item) = find_item(map, &item_ref.placement, &item_ref.guid)
    {
        let theirs = item
            .fluid
            .filter(|_| item.config.has_flag(ItemFlag::LiquidContainer));
        let aimed = match (item.config.attr_fluid_source(), theirs) {
            (Some(fluid), _) => Some(Aim::Fill(fluid)),
            (None, Some(theirs)) => Some(Aim::Transfer(item_ref.clone(), theirs)),
            (None, None) => None,
        };
        if let Some(aimed) = aimed {
            return match at.placement_is_adjacent(&item_ref.placement) {
                true => Ok(aimed),
                false => Err(TOO_FAR),
            };
        }
    }
    let there = target.position.as_ref().ok_or(NOT_POSSIBLE)?;
    pour_at(map, at, there)
}

fn pour_at(map: &GameMap, at: &Position, there: &Position) -> Result<Aim, &'static str> {
    if !at.is_within(there, 1) {
        return Err(TOO_FAR);
    }
    if !takes_a_pool(map, there) {
        return Err(NOT_POSSIBLE);
    }
    Ok(Aim::Pour(there.clone()))
}

fn takes_a_pool(map: &GameMap, pos: &Position) -> bool {
    map.iter_items(pos).is_ok_and(|items| {
        let items: Vec<&Item> = items.collect();
        items
            .first()
            .is_some_and(|item| item.config.has_flag(ItemFlag::Ground))
            && !items
                .iter()
                .any(|item| item.config.has_flag(ItemFlag::Unpass))
    })
}

fn refuse(ctx: &mut TickCtx, agent_key: AgentKey, message: &str) -> Result<(), ItemActionError> {
    ctx.events.push(BroadcastMessage::UseItemDenied {
        agent_key,
        message: message.to_owned(),
    });
    Err(ItemActionError::Reported)
}

fn drink(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    source: &ItemRef,
    fluid: FluidType,
) -> Result<(), ItemActionError> {
    let position = ctx
        .map
        .agent_position(agent_key)
        .cloned()
        .ok_or(ItemActionError::InvalidState)?;
    refill(ctx, source, FluidType::None)?;
    ctx.events.push(BroadcastMessage::AgentActionMessage {
        position,
        message: drink_message(fluid).to_owned(),
    });
    Ok(())
}

fn drink_message(fluid: FluidType) -> &'static str {
    match fluid {
        FluidType::Wine | FluidType::Beer | FluidType::Rum | FluidType::Mead => "Aah...",
        FluidType::Slime | FluidType::Urine | FluidType::Ink => "Urgh!",
        FluidType::Mana | FluidType::Health => "Aaaah...",
        FluidType::Lemonade
        | FluidType::FruitJuice
        | FluidType::CoconutMilk
        | FluidType::Candy
        | FluidType::Chocolate => "Mmmh.",
        _ => "Gulp.",
    }
}

fn pour(
    ctx: &mut TickCtx,
    source: &ItemRef,
    fluid: FluidType,
    at: &Position,
) -> Result<(), ItemActionError> {
    refill(ctx, source, FluidType::None)?;
    match place_pool(ctx, at, fluid) {
        true => Ok(()),
        false => Err(ItemActionError::ActionFailed),
    }
}

/// Takes the container out and puts it back in the same place holding `fluid`, so the change is
/// recorded like any other write.
fn refill(ctx: &mut TickCtx, container: &ItemRef, fluid: FluidType) -> Result<(), ItemActionError> {
    let (mut item, index) =
        remove_item_at(ctx, container, 1).map_err(|_| ItemActionError::ActionFailed)?;
    item.fluid = Some(fluid);
    insert_item_at(ctx, item, &container.placement, index)
        .map_err(|_| ItemActionError::ActionFailed)
}

/// Lays a pool of `fluid` on `pos` above its ground, replacing a pool already there. `false` when
/// nothing could be placed.
pub(super) fn place_pool(ctx: &mut TickCtx, pos: &Position, fluid: FluidType) -> bool {
    let Some(config) = ITEM_CONFIGS.get(&GAME_CONFIG.combat.pool_item_id) else {
        return false;
    };
    let (ground_depth, existing) = ctx
        .map
        .iter_items(pos)
        .map(|items| {
            let items: Vec<_> = items.collect();
            let depth = items
                .iter()
                .take_while(|it| it.config.has_flag(ItemFlag::Ground))
                .count();
            let existing = items
                .iter()
                .find(|it| it.config.has_flag(ItemFlag::LiquidPool))
                .map(|it| it.guid);
            (depth, existing)
        })
        .unwrap_or((0, None));

    let pool = Item::new_fluid(config.clone(), fluid);
    let tick = ctx.tick;
    let Ok(item) = ctx.map.place_item(pos, Some(ground_depth), None, pool) else {
        return false;
    };
    check_decay(ctx.scheduled, item, ItemPlacement::Map(pos.clone()), tick);
    if let Some(guid) = existing {
        ctx.map.remove_item_from_tile(pos, &guid, 1);
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::entities::{
        agent::Agent,
        items::{ItemAttribute, ItemConfig, ItemId},
        map::MapTile,
        world_map::WorldMap,
    };
    use crate::game::{TestHarness, item_multi_action::use_item_with};
    use crate::persistence::test_fixtures::{a_test_creature, a_test_snapshot};

    const HERE: Position = Position { x: 10, y: 10, z: 7 };
    const THERE: Position = Position { x: 10, y: 11, z: 7 };
    const FAR: Position = Position { x: 10, y: 13, z: 7 };
    const BARE: Position = Position { x: 11, y: 10, z: 7 };

    fn an_item(id: u16, flags: Vec<ItemFlag>, attributes: Vec<ItemAttribute>) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(id),
                format!("item {id}"),
                None,
                None,
                flags,
                attributes,
            )),
            1,
        )
    }

    fn ground() -> Item {
        an_item(64000, vec![ItemFlag::Ground], vec![])
    }

    fn a_vial(fluid: FluidType) -> Item {
        let mut vial = an_item(
            64001,
            vec![
                ItemFlag::Usable,
                ItemFlag::Multiuse,
                ItemFlag::Take,
                ItemFlag::LiquidContainer,
            ],
            vec![],
        );
        vial.fluid = Some(fluid);
        vial
    }

    fn a_well(fluid: FluidType) -> Item {
        an_item(64002, vec![], vec![ItemAttribute::FluidSource(fluid)])
    }

    fn a_wall() -> Item {
        an_item(64003, vec![ItemFlag::Unpass], vec![])
    }

    fn a_pool(fluid: FluidType) -> Item {
        Item::new_fluid(
            Arc::clone(&ITEM_CONFIGS[&GAME_CONFIG.combat.pool_item_id]),
            fluid,
        )
    }

    fn on_map(item: &Item, pos: Position) -> ItemRef {
        ItemRef {
            guid: item.guid,
            placement: ItemPlacement::Map(pos),
        }
    }

    fn on_tile(pos: Position) -> UseTarget {
        UseTarget {
            item: None,
            agent: None,
            position: Some(pos),
        }
    }

    fn on_item(item: &ItemRef) -> UseTarget {
        let ItemPlacement::Map(pos) = &item.placement else {
            unreachable!("the scenes only place items on the map");
        };
        UseTarget {
            item: Some(item.clone()),
            agent: None,
            position: Some(pos.clone()),
        }
    }

    struct Scene {
        map: WorldMap,
        h: TestHarness,
        user: AgentKey,
    }

    impl Scene {
        /// The user stands on `HERE`, which must be among `tiles`.
        fn new(tiles: Vec<(Position, Vec<Item>)>) -> Self {
            let mut map = GameMap::new();
            for (pos, items) in tiles {
                let mut tile = MapTile::new();
                for item in items {
                    tile.push_item(item);
                }
                map.insert_tile(pos, tile);
            }
            let user = map
                .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &HERE)
                .unwrap();
            Scene {
                map: WorldMap::new(map),
                h: TestHarness::new(),
                user,
            }
        }

        fn myself(&self) -> UseTarget {
            UseTarget {
                item: None,
                agent: Some(self.user),
                position: Some(HERE),
            }
        }

        fn use_on(&mut self, source: &ItemRef, target: UseTarget) -> Result<(), ItemActionError> {
            use_fluid(&mut self.h.ctx(&mut self.map), self.user, source, &target)
        }

        fn fluid_of(&self, item: &ItemRef) -> Option<FluidType> {
            find_item(&self.map, &item.placement, &item.guid).and_then(|item| item.fluid)
        }

        fn pools_at(&self, pos: &Position) -> Vec<Option<FluidType>> {
            self.map
                .iter_items(pos)
                .unwrap()
                .filter(|item| item.config.has_flag(ItemFlag::LiquidPool))
                .map(|item| item.fluid)
                .collect()
        }

        fn denied(&self) -> Vec<&str> {
            self.h
                .events
                .iter()
                .filter_map(|event| match event {
                    BroadcastMessage::UseItemDenied { message, .. } => Some(message.as_str()),
                    _ => None,
                })
                .collect()
        }
    }

    #[test]
    fn drinking_empties_the_container_and_says_how_it_tasted() {
        let vial = a_vial(FluidType::Wine);
        let source = on_map(&vial, HERE);
        let mut s = Scene::new(vec![(HERE, vec![ground(), vial])]);

        let target = s.myself();
        s.use_on(&source, target).unwrap();

        assert_eq!(s.fluid_of(&source), Some(FluidType::None));
        assert!(s.h.events.iter().any(|event| matches!(
            event,
            BroadcastMessage::AgentActionMessage { position, message }
                if *position == HERE && message == "Aah..."
        )));
    }

    #[test]
    fn each_fluid_tastes_like_its_group() {
        assert_eq!(drink_message(FluidType::Beer), "Aah...");
        assert_eq!(drink_message(FluidType::Ink), "Urgh!");
        assert_eq!(drink_message(FluidType::Health), "Aaaah...");
        assert_eq!(drink_message(FluidType::Candy), "Mmmh.");
        assert_eq!(drink_message(FluidType::Water), "Gulp.");
    }

    #[test]
    fn an_empty_container_can_be_neither_drunk_nor_poured() {
        let vial = a_vial(FluidType::None);
        let source = on_map(&vial, HERE);
        let mut s = Scene::new(vec![(HERE, vec![ground(), vial]), (THERE, vec![ground()])]);

        let target = s.myself();
        let drunk = s.use_on(&source, target);
        let poured = s.use_on(&source, on_tile(THERE));

        assert!(matches!(drunk, Err(ItemActionError::Reported)));
        assert!(matches!(poured, Err(ItemActionError::Reported)));
        assert_eq!(s.denied(), [EMPTY, EMPTY]);
        assert!(s.pools_at(&THERE).is_empty());
    }

    #[test]
    fn pouring_leaves_a_pool_of_the_fluid_and_empties_the_container() {
        let vial = a_vial(FluidType::Mud);
        let source = on_map(&vial, HERE);
        let mut s = Scene::new(vec![(HERE, vec![ground(), vial]), (THERE, vec![ground()])]);

        s.use_on(&source, on_tile(THERE)).unwrap();

        assert_eq!(s.pools_at(&THERE), [Some(FluidType::Mud)]);
        assert_eq!(s.fluid_of(&source), Some(FluidType::None));
    }

    #[test]
    fn pouring_on_another_creature_pours_on_its_tile() {
        let vial = a_vial(FluidType::Blood);
        let source = on_map(&vial, HERE);
        let mut s = Scene::new(vec![(HERE, vec![ground(), vial]), (THERE, vec![ground()])]);
        let rat = s
            .map
            .insert_agent(a_test_creature("Rat", 100, (1, 1)), &THERE)
            .unwrap();

        let target = UseTarget {
            item: None,
            agent: Some(rat),
            position: Some(THERE),
        };
        s.use_on(&source, target).unwrap();

        assert_eq!(s.pools_at(&THERE), [Some(FluidType::Blood)]);
    }

    #[test]
    fn a_new_pool_replaces_the_one_already_there() {
        let vial = a_vial(FluidType::Water);
        let source = on_map(&vial, HERE);
        let mut s = Scene::new(vec![
            (HERE, vec![ground(), vial]),
            (THERE, vec![ground(), a_pool(FluidType::Slime)]),
        ]);

        s.use_on(&source, on_tile(THERE)).unwrap();

        assert_eq!(s.pools_at(&THERE), [Some(FluidType::Water)]);
    }

    #[test]
    fn pouring_out_of_reach_or_where_a_pool_cannot_lie_is_refused() {
        let vial = a_vial(FluidType::Water);
        let source = on_map(&vial, HERE);
        let mut s = Scene::new(vec![
            (HERE, vec![ground(), vial]),
            (THERE, vec![ground(), a_wall()]),
            (FAR, vec![ground()]),
            (BARE, vec![]),
        ]);

        for pos in [FAR, THERE, BARE] {
            let _ = s.use_on(&source, on_tile(pos));
        }

        assert_eq!(s.denied(), [TOO_FAR, NOT_POSSIBLE, NOT_POSSIBLE]);
        assert_eq!(s.fluid_of(&source), Some(FluidType::Water));
    }

    #[test]
    fn a_fluid_source_fills_the_container_whatever_it_held() {
        let vial = a_vial(FluidType::Wine);
        let well = a_well(FluidType::Water);
        let (source, target) = (on_map(&vial, HERE), on_map(&well, THERE));
        let mut s = Scene::new(vec![
            (HERE, vec![ground(), vial]),
            (THERE, vec![ground(), well]),
        ]);

        s.use_on(&source, on_item(&target)).unwrap();

        assert_eq!(s.fluid_of(&source), Some(FluidType::Water));
    }

    #[test]
    fn a_target_item_out_of_reach_is_refused() {
        let vial = a_vial(FluidType::None);
        let well = a_well(FluidType::Water);
        let (source, target) = (on_map(&vial, HERE), on_map(&well, FAR));
        let mut s = Scene::new(vec![
            (HERE, vec![ground(), vial]),
            (FAR, vec![ground(), well]),
        ]);

        let _ = s.use_on(&source, on_item(&target));

        assert_eq!(s.denied(), [TOO_FAR]);
        assert_eq!(s.fluid_of(&source), Some(FluidType::None));
    }

    #[test]
    fn a_full_container_fills_an_empty_one_and_takes_it_back() {
        let full = a_vial(FluidType::Milk);
        let empty = a_vial(FluidType::None);
        let (a, b) = (on_map(&full, HERE), on_map(&empty, THERE));
        let mut s = Scene::new(vec![
            (HERE, vec![ground(), full]),
            (THERE, vec![ground(), empty]),
        ]);

        s.use_on(&a, on_item(&b)).unwrap();
        assert_eq!(
            (s.fluid_of(&a), s.fluid_of(&b)),
            (Some(FluidType::None), Some(FluidType::Milk))
        );

        s.use_on(&a, on_item(&b)).unwrap();
        assert_eq!(
            (s.fluid_of(&a), s.fluid_of(&b)),
            (Some(FluidType::Milk), Some(FluidType::None))
        );
    }

    #[test]
    fn two_full_or_two_empty_containers_cannot_trade_fluids() {
        let (beer, wine) = (a_vial(FluidType::Beer), a_vial(FluidType::Wine));
        let (dry, parched) = (a_vial(FluidType::None), a_vial(FluidType::None));
        let (beer_ref, wine_ref) = (on_map(&beer, HERE), on_map(&wine, THERE));
        let (dry_ref, parched_ref) = (on_map(&dry, HERE), on_map(&parched, THERE));
        let mut s = Scene::new(vec![
            (HERE, vec![ground(), beer, dry]),
            (THERE, vec![ground(), wine, parched]),
        ]);

        let _ = s.use_on(&beer_ref, on_item(&wine_ref));
        let _ = s.use_on(&dry_ref, on_item(&parched_ref));

        assert_eq!(s.denied(), [NOT_POSSIBLE, EMPTY]);
        assert_eq!(s.fluid_of(&beer_ref), Some(FluidType::Beer));
        assert_eq!(s.fluid_of(&wine_ref), Some(FluidType::Wine));
    }

    #[test]
    fn use_with_routes_a_liquid_container_here() {
        let vial = a_vial(FluidType::Tea);
        let source = on_map(&vial, HERE);
        let mut s = Scene::new(vec![(HERE, vec![ground(), vial])]);

        let target = s.myself();
        use_item_with(&mut s.h.ctx(&mut s.map), s.user, source.clone(), target);

        assert_eq!(s.fluid_of(&source), Some(FluidType::None));
    }
}
