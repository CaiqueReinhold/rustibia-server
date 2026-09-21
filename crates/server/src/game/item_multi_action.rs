use smallvec::SmallVec;
use tracing::{error, warn};

use crate::{
    entities::{
        Bounds,
        agent::AgentKey,
        healing::{HealPlan, Restore},
        items::{Item, ItemFlag, ItemId, ItemMultiAction, ItemRef},
        map::GameMap,
        position::{ItemPlacement, Position},
        spells::Spell,
        targeting::AreaTarget,
    },
    game::{
        Mark, TickCtx,
        config::GAME_CONFIG,
        events::BroadcastMessage,
        healing::execute_healing,
        item_action::{ItemActionError, transform},
        item_movement::{insert_item_at, remove_item_at, return_item},
        map_query::find_item,
        spells::{CastSource, cast_spell},
    },
    persistence::{items::ITEM_CONFIGS, spells::SPELLS},
};

#[derive(Debug)]
pub struct UseTarget {
    pub item: Option<ItemRef>,
    pub agent: Option<AgentKey>,
    pub position: Option<Position>,
}

pub fn use_item_with(ctx: &mut TickCtx, agent_key: AgentKey, source: ItemRef, target: UseTarget) {
    let mark = ctx.mark();

    if ctx
        .map
        .get_agent(agent_key)
        .map(|agent| agent.next_use_tick > ctx.tick)
        .unwrap_or(false)
    {
        return use_item_failed(ctx, mark, agent_key, "Can't use that fast");
    }

    let Some(source_item) = find_item(ctx.map, &source.placement, &source.guid) else {
        return use_item_failed(ctx, mark, agent_key, "Item was not found");
    };
    let source_item_id = source_item.id();
    let source_is_usable = source_item.config.has_flag(ItemFlag::Usable);
    // Two sources, and they answer different questions. What a shovel does is a
    // property of the world, so it stays in `game_conf.yaml`'s id lists; what a
    // potion restores is a property of the item, so it rides in the catalogue.
    let action = GAME_CONFIG
        .multi_action
        .tool_action(source_item_id)
        .or_else(|| source_item.config.attr_multi_action());

    if ctx
        .map
        .agent_position(agent_key)
        .filter(|player_pos| player_pos.placement_is_adjacent(&source.placement))
        .is_none()
    {
        return use_item_failed(ctx, mark, agent_key, "Item is too far");
    }

    if !source_is_usable {
        return use_item_failed(ctx, mark, agent_key, "Can't use that");
    }

    let Some(action) = action else {
        return use_item_failed(ctx, mark, agent_key, "Can't use that");
    };

    match route_multi_action(ctx, &action, agent_key, &source, &target) {
        Ok(()) => {
            ctx.map
                .agent_mut(agent_key)
                .unwrap()
                .stamp_use(ctx.tick + GAME_CONFIG.action.use_item_cooldown_ticks);
        }
        Err(ItemActionError::Reported) => {}
        Err(e) => {
            if let ItemActionError::InvalidState = e {
                warn!("{e}");
            }
            use_item_failed(ctx, mark, agent_key, "Can't use that");
        }
    }
}

/// Discards whatever a half-finished use reported and announces the refusal in its place.
fn use_item_failed(ctx: &mut TickCtx, mark: Mark, agent_key: AgentKey, message: &str) {
    ctx.rollback_to(mark);
    ctx.events.push(BroadcastMessage::UseItemDenied {
        agent_key,
        message: message.to_owned(),
    });
}

fn route_multi_action(
    ctx: &mut TickCtx,
    action: &ItemMultiAction,
    agent_key: AgentKey,
    source: &ItemRef,
    target: &UseTarget,
) -> Result<(), ItemActionError> {
    match action {
        ItemMultiAction::Shovel => {
            let tool_target = tool_target(ctx.map, target)?;
            shovel(ctx, tool_target)
        }
        ItemMultiAction::Rope => {
            let tool_target = tool_target(ctx.map, target)?;
            rope(ctx, agent_key, tool_target)
        }
        ItemMultiAction::Potion {
            health,
            mana,
            flask,
        } => potion(
            ctx,
            agent_key,
            target.agent.ok_or(ItemActionError::NoTarget)?,
            source,
            *health,
            *mana,
            *flask,
        ),
        ItemMultiAction::Rune { spell } => match SPELLS.get(spell) {
            Some(found) => rune(ctx, agent_key, source, found, target),
            None => {
                error!("rune {source:?} names spell {spell:?}, which is not in the catalogue");
                Err(ItemActionError::ActionFailed)
            }
        },
    }
}

fn rune_target(target: &UseTarget) -> AreaTarget {
    match (target.agent, target.position.as_ref()) {
        (Some(key), _) => AreaTarget::Agent(key),
        (None, Some(position)) => AreaTarget::Position(position.clone()),
        (None, None) => AreaTarget::None,
    }
}

fn rune(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    source: &ItemRef,
    spell: &Spell,
    target: &UseTarget,
) -> Result<(), ItemActionError> {
    cast_spell(
        ctx,
        agent_key,
        spell,
        rune_target(target),
        None,
        CastSource::Rune,
    )
    .map_err(|_| ItemActionError::Reported)?;

    if remove_item_at(ctx, source, 1).is_err() {
        error!("could not spend rune {source:?} after its cast succeeded");
    }
    Ok(())
}

fn tool_target<'a>(map: &GameMap, target: &'a UseTarget) -> Result<&'a ItemRef, ItemActionError> {
    let item = target.item.as_ref().ok_or(ItemActionError::ActionFailed)?;
    if find_item(map, &item.placement, &item.guid).is_none() {
        return Err(ItemActionError::ActionFailed);
    }
    Ok(item)
}

fn shovel(ctx: &mut TickCtx, target: &ItemRef) -> Result<(), ItemActionError> {
    let target_item_id = find_item(ctx.map, &target.placement, &target.guid)
        .unwrap()
        .id();
    if !GAME_CONFIG
        .multi_action
        .diggable_ids
        .contains(&target_item_id)
    {
        return Err(ItemActionError::ActionFailed);
    }
    transform(ctx, target, ItemId(target_item_id.0 + 1))
}

fn first_available_position_up(
    map: &GameMap,
    pos: &Position,
    agent_key: AgentKey,
) -> Option<Position> {
    [
        Position::new(pos.x, pos.y.saturating_sub(1), pos.z - 1),
        Position::new(pos.x, pos.y.saturating_add(1), pos.z - 1),
        Position::new(pos.x.saturating_sub(1), pos.y, pos.z - 1),
        Position::new(pos.x.saturating_add(1), pos.y, pos.z - 1),
    ]
    .iter()
    .find(|try_pos| map.can_move(try_pos, agent_key))
    .cloned()
}

fn rope(ctx: &mut TickCtx, agent_key: AgentKey, target: &ItemRef) -> Result<(), ItemActionError> {
    let target_item_id = find_item(ctx.map, &target.placement, &target.guid)
        .unwrap()
        .id();
    let ItemPlacement::Map(pos) = &target.placement else {
        return Err(ItemActionError::ActionFailed);
    };
    let Some(target_pos) = first_available_position_up(ctx.map, pos, agent_key) else {
        return Err(ItemActionError::InvalidState);
    };

    if GAME_CONFIG
        .multi_action
        .rope_spot_ids
        .contains(&target_item_id)
    {
        ctx.map
            .move_agent(agent_key, &target_pos)
            .map_err(|_| ItemActionError::ActionFailed)?;
        ctx.events.push(BroadcastMessage::AgentTeleported {
            agent_key,
            from_position: pos.clone(),
            to_position: target_pos,
        });
        return Ok(());
    } else if GAME_CONFIG
        .multi_action
        .opened_hole_ids
        .contains(&target_item_id)
    {
        let down = Position::new(pos.x, pos.y, pos.z + 1);
        let last_agent = ctx
            .map
            .iter_agents_at(&down)
            .ok()
            .and_then(|mut agents_iter| agents_iter.next().cloned());
        if let Some(last_agent) = last_agent {
            if ctx.map.move_agent(last_agent, &target_pos).is_err() {
                return Err(ItemActionError::ActionFailed);
            }
            ctx.events.push(BroadcastMessage::AgentTeleported {
                agent_key: last_agent,
                from_position: pos.clone(),
                to_position: target_pos,
            });
            return Ok(());
        }

        let top_item = ctx
            .map
            .get_top_item(&down)
            .map(|item| (item.guid, item.amount));
        if let Some((guid, amount)) = top_item {
            let hauled = remove_item_at(
                ctx,
                &ItemRef {
                    guid,
                    placement: ItemPlacement::Map(down),
                },
                amount,
            )
            .and_then(|(removed_item, index)| {
                insert_item_at(ctx, removed_item, &ItemPlacement::Map(target_pos), index)
            });
            if hauled.is_err() {
                return Err(ItemActionError::ActionFailed);
            }
            return Ok(());
        }
    }

    Err(ItemActionError::ActionFailed)
}

fn potion(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    target: AgentKey,
    potion: &ItemRef,
    health: Option<Bounds>,
    mana: Option<Bounds>,
    flask: Option<ItemId>,
) -> Result<(), ItemActionError> {
    if remove_item_at(ctx, potion, 1).is_err() {
        return Err(ItemActionError::ActionFailed);
    }
    if let Some(flask) = flask {
        match ITEM_CONFIGS.get(&flask) {
            Some(config) => {
                let flask = Item::new(config.clone(), 1);
                if let Err(e) = return_item(ctx, agent_key, &potion.placement, None, flask) {
                    error!("could not give the empty flask to {agent_key:?}: {e}");
                }
            }
            None => error!("potion returns flask {flask}, which is not in the catalogue"),
        }
    }

    let life = health.map(|b| ctx.roll.uniform(b.min, b.max));
    let mana = mana.map(|b| ctx.roll.uniform(b.min, b.max));
    let plan = HealPlan {
        caster: agent_key,
        restores: SmallVec::from([(target, Restore { life, mana })]),
        area_effect: None,
    };
    execute_healing(ctx, plan);

    if let Some(position) = ctx.map.agent_position(target).cloned() {
        ctx.events
            .push(BroadcastMessage::PotionDrunk { target, position });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::items::MAX_STACK_AMOUNT;
    use crate::entities::combat::CombatElement;
    use crate::entities::effects::EffectId;
    use crate::entities::spells::{
        PowerCurve, Spell, SpellAttack, SpellDelivery, SpellEffect, SpellGroup, SpellHealing,
        SpellId,
    };
    use crate::entities::targeting::TargetMode;
    use crate::entities::world_map::WorldMap;
    use crate::entities::{
        agent::{Agent, Pool},
        healing::RestoreType,
        inventory::InventorySlot,
        items::{Item, ItemAttribute, ItemConfig, ItemId},
        map::MapTile,
    };
    use crate::game::spells::SpellCastingDenyReason;
    use crate::game::{TestHarness, Tick};
    use crate::persistence::items::ITEM_CONFIGS;
    use crate::persistence::test_fixtures::{a_spell, a_test_creature, a_test_snapshot};
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    fn an_item(id: ItemId) -> Item {
        Item::new(
            ITEM_CONFIGS
                .get(&id)
                .unwrap_or_else(|| panic!("item {id} missing from the shipped catalogue"))
                .clone(),
            1,
        )
    }

    fn a_built_item(id: ItemId, flags: impl IntoIterator<Item = ItemFlag>) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                id,
                format!("item {id}"),
                None,
                None,
                flags,
                Vec::new(),
            )),
            1,
        )
    }

    /// `potion` resolves the flask it returns through `ITEM_CONFIGS`, so the flask has to
    /// be one the catalogue carries.
    fn a_flask_id() -> ItemId {
        ITEM_CONFIGS
            .values()
            .filter(|config| {
                config.has_flag(ItemFlag::Cumulative) && config.has_flag(ItemFlag::Take)
            })
            .map(|config| config.id)
            .min()
            .expect("the catalogue carries no stackable item a potion could return")
    }

    fn a_tile_with(item: Item) -> MapTile {
        let mut tile = MapTile::new();
        tile.push_item(item);
        tile
    }

    /// A configured id that is absent from the catalogue, or that lost `usable`, reaches
    /// `use_item_with` and is refused with "Can't use that" and no error anywhere.
    #[test]
    fn every_configured_tool_exists_and_is_usable() {
        let config = &GAME_CONFIG.multi_action;
        assert!(!config.shovel_ids.is_empty(), "no shovel is configured");
        assert!(!config.rope_ids.is_empty(), "no rope is configured");

        for (ids, expected) in [
            (&config.shovel_ids, ItemMultiAction::Shovel),
            (&config.rope_ids, ItemMultiAction::Rope),
        ] {
            for id in ids {
                let item = ITEM_CONFIGS
                    .get(id)
                    .unwrap_or_else(|| panic!("tool {id} is not in the catalogue"));
                assert!(
                    item.has_flag(ItemFlag::Usable),
                    "tool {id} ({}) lost its usable flag",
                    item.name
                );
                assert_eq!(config.tool_action(*id), Some(expected));
            }
        }
    }

    /// `usable + multiuse` is what separates the tools from the rope bridges, railings and
    /// decorations sharing their names; a tool the config omits is inert, and silently so.
    #[test]
    fn every_tool_in_the_catalogue_is_configured() {
        let config = &GAME_CONFIG.multi_action;
        let mut unconfigured = Vec::new();

        for (id, item) in ITEM_CONFIGS.iter() {
            let is_tool = item.has_flag(ItemFlag::Usable) && item.has_flag(ItemFlag::Multiuse);
            let named = item.name.contains("shovel") || item.name.contains("rope");
            if is_tool && named && config.tool_action(*id).is_none() {
                unconfigured.push((*id, item.name.clone()));
            }
        }

        unconfigured.sort();
        assert!(
            unconfigured.is_empty(),
            "usable multiuse tools missing from game_conf.yaml: {unconfigured:?}"
        );
    }

    /// `shovel` digs into `item_id + 1`, and `transform` refuses an id the catalogue does
    /// not carry, so an unpaired diggable is a dig that silently does nothing.
    #[test]
    fn every_diggable_digs_into_an_existing_item() {
        for id in &GAME_CONFIG.multi_action.diggable_ids {
            let into = ItemId(id.0 + 1);
            assert!(
                ITEM_CONFIGS.contains_key(&into),
                "diggable {id:?} digs into {into:?}, which is not in the catalogue"
            );
        }
    }

    /// Reports whether the action was denied, and what is left on the target tile.
    fn use_tool_on(tool_id: ItemId, target_id: ItemId) -> (bool, Option<Item>) {
        let mut h = TestHarness::seeded(1);
        let (here, there) = (Position::new(10, 10, 7), Position::new(10, 11, 7));
        let mut map = GameMap::new();

        let (tool, target) = (an_item(tool_id), an_item(target_id));
        let (tool_guid, target_guid) = (tool.guid, target.guid);
        map.insert_tile(here.clone(), a_tile_with(tool));
        map.insert_tile(there.clone(), a_tile_with(target));
        let agent = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();

        let mut map = WorldMap::new(map);
        use_item_with(
            &mut h.ctx(&mut map),
            agent,
            ItemRef {
                guid: tool_guid,
                placement: ItemPlacement::Map(here),
            },
            UseTarget {
                item: Some(ItemRef {
                    guid: target_guid,
                    placement: ItemPlacement::Map(there.clone()),
                }),
                agent: None,
                position: None,
            },
        );

        let denied = h
            .events
            .iter()
            .any(|b| matches!(b, BroadcastMessage::UseItemDenied { .. }));
        (denied, map.get_top_item(&there).cloned())
    }

    #[test]
    fn every_shovel_digs_every_diggable_into_a_hole() {
        let config = &GAME_CONFIG.multi_action;
        for shovel_id in &config.shovel_ids {
            for diggable_id in &config.diggable_ids {
                let (denied, dug) = use_tool_on(*shovel_id, *diggable_id);
                let name = &ITEM_CONFIGS[shovel_id].name;

                assert!(!denied, "{name} ({shovel_id}) could not dig {diggable_id}");
                let dug = dug.unwrap_or_else(|| {
                    panic!("{name} ({shovel_id}) left {diggable_id}'s tile empty")
                });
                assert_eq!(
                    dug.id(),
                    ItemId(diggable_id.0 + 1),
                    "{name} ({shovel_id:?}) dug {diggable_id:?} into the wrong item"
                );
            }
        }
    }

    #[test]
    fn every_rope_hauls_the_player_up_a_rope_spot() {
        let mut h = TestHarness::seeded(1);
        let config = &GAME_CONFIG.multi_action;
        let rope_spot = config.rope_spot_ids[0];

        for rope_id in &config.rope_ids {
            let (here, spot) = (Position::new(10, 10, 7), Position::new(10, 11, 7));
            let above = Position::new(10, 10, 6);
            let mut map = GameMap::new();

            let (rope, target) = (an_item(*rope_id), an_item(rope_spot));
            let (rope_guid, target_guid) = (rope.guid, target.guid);
            map.insert_tile(here.clone(), a_tile_with(rope));
            map.insert_tile(spot.clone(), a_tile_with(target));
            // The only tile `firt_available_position_up` can find, so the destination is known.
            map.insert_tile(above.clone(), a_tile_with(an_item(rope_spot)));
            let agent = map
                .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
                .unwrap();

            let mut map = WorldMap::new(map);
            use_item_with(
                &mut h.ctx(&mut map),
                agent,
                ItemRef {
                    guid: rope_guid,
                    placement: ItemPlacement::Map(here),
                },
                UseTarget {
                    item: Some(ItemRef {
                        guid: target_guid,
                        placement: ItemPlacement::Map(spot),
                    }),
                    agent: None,
                    position: None,
                },
            );

            let name = &ITEM_CONFIGS[rope_id].name;
            assert!(
                !h.events
                    .iter()
                    .any(|b| matches!(b, BroadcastMessage::UseItemDenied { .. })),
                "{name} ({rope_id}) was refused: {:?}",
                h.events
            );
            assert_eq!(
                map.agent_position(agent),
                Some(&above),
                "{name} ({rope_id}) did not haul the player up"
            );
        }
    }

    #[test]
    fn a_non_tool_cannot_be_used_on_a_diggable() {
        let mut h = TestHarness::seeded(1);
        let (here, sand_pos) = (Position::new(10, 10, 7), Position::new(10, 11, 7));
        let hammer = a_built_item(
            ItemId(9998),
            [ItemFlag::Usable, ItemFlag::Multiuse, ItemFlag::Take],
        );
        assert_eq!(GAME_CONFIG.multi_action.tool_action(hammer.id()), None);

        let mut map = GameMap::new();
        let sand = a_built_item(
            GAME_CONFIG.multi_action.diggable_ids[0],
            [ItemFlag::Ground, ItemFlag::Usable],
        );
        let (hammer_guid, sand_guid) = (hammer.guid, sand.guid);
        map.insert_tile(here.clone(), a_tile_with(hammer));
        map.insert_tile(sand_pos.clone(), a_tile_with(sand));
        let agent = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();

        let mut map = WorldMap::new(map);
        use_item_with(
            &mut h.ctx(&mut map),
            agent,
            ItemRef {
                guid: hammer_guid,
                placement: ItemPlacement::Map(here),
            },
            UseTarget {
                item: Some(ItemRef {
                    guid: sand_guid,
                    placement: ItemPlacement::Map(sand_pos.clone()),
                }),
                agent: None,
                position: None,
            },
        );

        assert!(
            h.events
                .iter()
                .any(|b| matches!(b, BroadcastMessage::UseItemDenied { .. })),
        );
        assert!(map.get_item_by_id(&sand_pos, &sand_guid).is_some());
    }

    // ── Potions ───────────────────────────────────────────────────────────────

    /// Built here rather than looked up: these tests are about what `drink` does
    /// with the bounds it is handed, not about the amounts the catalogue happens to
    /// carry. Id 9999 is in no tool list in `game_conf.yaml` either, so reaching
    /// this at all proves the catalogue is a real second source for the lookup.
    fn a_potion(
        amount: u8,
        health: Option<Bounds>,
        mana: Option<Bounds>,
        flask: Option<ItemId>,
    ) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(9999),
                "test potion".to_string(),
                None,
                None,
                HashSet::from([
                    ItemFlag::Usable,
                    ItemFlag::Multiuse,
                    ItemFlag::Cumulative,
                    ItemFlag::Take,
                ]),
                [ItemAttribute::MultiAction(ItemMultiAction::Potion {
                    health,
                    mana,
                    flask,
                })]
                .to_vec(),
            )),
            amount,
        )
    }

    enum Target {
        Myself,
        Other,
        Creature,
        Nobody,
    }

    fn a_plain_item() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(9998),
                "something to click".to_string(),
                None,
                None,
                HashSet::new(),
                Vec::new(),
            )),
            1,
        )
    }

    fn bounds(min: u32, max: u32) -> Option<Bounds> {
        Some(Bounds { min, max })
    }

    fn pool(current: u32, maximum: u32) -> Pool {
        Pool { current, maximum }
    }

    struct Drunk {
        user: AgentKey,
        denied: bool,
        life: Pool,
        mana: Pool,
        target_life: Pool,
        charges_left: Option<u8>,
        flasks_at_the_users_feet: usize,
        at_the_users_feet: Vec<(ItemId, u8)>,
        broadcasts: Vec<BroadcastMessage>,
    }

    impl Drunk {
        fn broadcast_kinds(&self) -> Vec<&'static str> {
            self.broadcasts
                .iter()
                .filter_map(|b| match b {
                    BroadcastMessage::AgentHealed {
                        restore_type: RestoreType::Life,
                        ..
                    } => Some("life"),
                    BroadcastMessage::AgentHealed {
                        restore_type: RestoreType::Mana,
                        ..
                    } => Some("mana"),
                    BroadcastMessage::UseItemDenied { .. } => Some("denied"),
                    _ => None,
                })
                .collect()
        }
    }

    /// Puts the potion on the drinker's own tile and something else beside them to
    /// be the thing clicked, aims the use at `on`, then drinks `times` times at
    /// tick 0.
    fn drink_potion_on(potion: Item, life: Pool, mana: Pool, on: Target, times: u8) -> Drunk {
        let (here, there) = (Position::new(10, 10, 7), Position::new(10, 11, 7));
        let mut map = GameMap::new();

        let potion_guid = potion.guid;
        let clicked = a_plain_item();
        let clicked_guid = clicked.guid;
        map.insert_tile(here.clone(), a_tile_with(potion));
        map.insert_tile(there.clone(), a_tile_with(clicked));

        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.life = life;
        snapshot.mana = mana;
        let user = map
            .insert_agent(Agent::from_player(snapshot), &here)
            .unwrap();

        let target = match on {
            Target::Myself => Some(user),
            Target::Other => {
                let mut other = a_test_snapshot(2, 2);
                other.life = pool(10, 1000);
                other.mana = pool(50, 1000);
                Some(map.insert_agent(Agent::from_player(other), &there).unwrap())
            }
            Target::Creature => {
                let rat = map
                    .insert_agent(a_test_creature("Rat", 1000, (1, 1)), &there)
                    .unwrap();
                // Wounded to 10, matching the other targets. A creature spawns full, and a
                // full pool takes nothing -- a heal aimed at it would restore zero and the
                // test would pass against a potion that healed nobody.
                map.get_agent_mut(rat).unwrap().remove_life(990);
                Some(rat)
            }
            Target::Nobody => None,
        };

        let mut h = TestHarness::seeded(1);
        let mut map = WorldMap::new(map);
        for _ in 0..times {
            use_item_with(
                &mut h.ctx(&mut map),
                user,
                ItemRef {
                    guid: potion_guid,
                    placement: ItemPlacement::Map(here.clone()),
                },
                UseTarget {
                    item: Some(ItemRef {
                        guid: clicked_guid,
                        placement: ItemPlacement::Map(there.clone()),
                    }),
                    agent: target,
                    position: None,
                },
            );
        }

        let target_life = target
            .and_then(|t| map.get_agent(t))
            .map(|a| a.life().clone())
            .unwrap_or(pool(0, 0));

        Drunk {
            user,
            denied: h
                .events
                .iter()
                .any(|b| matches!(b, BroadcastMessage::UseItemDenied { .. })),
            life: map.get_agent(user).unwrap().life().clone(),
            mana: map.get_agent(user).unwrap().mana().clone(),
            target_life,
            charges_left: map
                .iter_items(&here)
                .ok()
                .and_then(|mut items| items.find(|i| i.id() == ItemId(9999)))
                .map(|item| item.amount),
            flasks_at_the_users_feet: map
                .iter_items(&here)
                .map(|items| items.filter(|i| i.id() != ItemId(9999)).count())
                .unwrap_or(0),
            at_the_users_feet: map
                .iter_items(&here)
                .map(|items| items.map(|i| (i.id(), i.amount)).collect())
                .unwrap_or_default(),
            broadcasts: h.events,
        }
    }

    #[test]
    fn a_health_potion_restores_life_within_its_bounds_and_spends_one_charge() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Myself,
            1,
        );

        assert!(!drunk.denied);
        assert!(
            (110..=210).contains(&drunk.life.current),
            "life {} is outside 10 + 100..200",
            drunk.life.current
        );
        assert_eq!(drunk.mana.current, 50, "a health potion touched the mana");
        assert_eq!(drunk.charges_left, Some(2));
        assert_eq!(drunk.broadcast_kinds(), ["life"]);
    }

    #[test]
    fn a_mana_potion_restores_mana_and_leaves_life_alone() {
        let drunk = drink_potion_on(
            a_potion(3, None, bounds(75, 125), None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Myself,
            1,
        );

        assert!(!drunk.denied);
        assert_eq!(drunk.life.current, 10, "a mana potion healed");
        assert!(
            (125..=175).contains(&drunk.mana.current),
            "mana {} is outside 50 + 75..125",
            drunk.mana.current
        );
        assert_eq!(drunk.charges_left, Some(2));
        assert_eq!(drunk.broadcast_kinds(), ["mana"]);
    }

    /// The whole reason the two pools live in one action: a spirit potion cannot be
    /// two actions on one item, so this is the shape that has to work.
    #[test]
    fn a_spirit_potion_restores_both_pools_from_one_charge() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(420, 580), bounds(180, 220), None),
            pool(100, 2000),
            pool(100, 2000),
            Target::Myself,
            1,
        );

        assert!(!drunk.denied);
        assert!(
            (520..=680).contains(&drunk.life.current),
            "life {}",
            drunk.life.current
        );
        assert!(
            (280..=320).contains(&drunk.mana.current),
            "mana {}",
            drunk.mana.current
        );
        assert_eq!(drunk.charges_left, Some(2), "one charge, both pools");
        assert_eq!(drunk.broadcast_kinds(), ["life", "mana"]);
    }

    #[test]
    fn neither_pool_can_be_filled_past_its_maximum() {
        let drunk = drink_potion_on(
            a_potion(1, bounds(500, 500), bounds(500, 500), None),
            pool(990, 1000),
            pool(995, 1000),
            Target::Myself,
            1,
        );

        assert_eq!(drunk.life.current, 1000);
        assert_eq!(drunk.mana.current, 1000);
    }

    /// TFS spends the flask regardless, and so does this. A potion that refused at
    /// full life would be a nicer rule and a different game.
    #[test]
    fn drinking_at_full_life_still_spends_the_charge() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(1000, 1000),
            pool(50, 1000),
            Target::Myself,
            1,
        );

        assert!(!drunk.denied);
        assert_eq!(drunk.life.current, 1000);
        assert_eq!(drunk.charges_left, Some(2));
    }

    /// `use_item_cooldown_ticks` is what stops a hotkey emptying a whole stack in
    /// one tick, and it is applied by `use_item_with` only when the action succeeded.
    #[test]
    fn a_second_drink_in_the_same_tick_is_refused() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Myself,
            2,
        );

        assert!(drunk.denied, "the second drink went through");
        assert_eq!(drunk.charges_left, Some(2), "two charges were spent");
        assert_eq!(drunk.broadcast_kinds(), ["life", "denied"]);
    }

    /// The last stone in the stack leaves no item behind, and the pools still move.
    #[test]
    fn the_last_charge_removes_the_potion_entirely() {
        let drunk = drink_potion_on(
            a_potion(1, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Myself,
            1,
        );

        assert!(!drunk.denied);
        assert_eq!(
            drunk.charges_left, None,
            "an empty stack was left on the tile"
        );
        assert!(drunk.life.current > 10);
    }

    fn attack_aimed(mut spell: Spell) -> Spell {
        spell.effect = SpellEffect::Attack(SpellAttack {
            target: TargetMode::Aimed,
            element: CombatElement::Fire,
            power: PowerCurve {
                base_power: 5.0,
                level_factor: 0.0,
                magic_factor: 0.0,
                melee_factor: 0.0,
                spread_min: 0.0,
                spread_max: 0.0,
                flat: 0.0,
            },
            effect_id: EffectId(1),
            missile_id: None,
            chain: None,
            weapon_required: false,
        });
        spell
    }

    fn a_rune(amount: u8, spell: SpellId) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(9998),
                "test rune".to_string(),
                None,
                None,
                HashSet::from([
                    ItemFlag::Usable,
                    ItemFlag::Multiuse,
                    ItemFlag::Cumulative,
                    ItemFlag::Take,
                ]),
                [ItemAttribute::MultiAction(ItemMultiAction::Rune { spell })].to_vec(),
            )),
            amount,
        )
    }

    fn throw_rune(
        spell: &Spell,
    ) -> (
        Result<(), ItemActionError>,
        Vec<BroadcastMessage>,
        Option<u8>,
        Tick,
        WorldMap,
        AgentKey,
    ) {
        let mut h = TestHarness::new();
        let here = Position::new(10, 10, 7);
        let mut map = GameMap::new();

        let item = a_rune(3, spell.id);
        let guid = item.guid;
        map.insert_tile(here.clone(), a_tile_with(item));
        let agent = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();

        let source = ItemRef {
            guid,
            placement: ItemPlacement::Map(here.clone()),
        };
        let mut map = WorldMap::new(map);
        let result = rune(
            &mut h.ctx(&mut map),
            agent,
            &source,
            spell,
            &UseTarget {
                item: None,
                agent: Some(agent),
                position: None,
            },
        );

        let left = map.get_top_item(&here).map(|item| item.amount);
        (result, h.events, left, h.tick, map, agent)
    }

    #[test]
    fn a_rune_aims_at_its_agent_before_its_tile() {
        let key = AgentKey::default();
        let here = Position::new(100, 100, 7);

        assert!(matches!(
            rune_target(&UseTarget {
                item: None,
                agent: Some(key),
                position: Some(here.clone()),
            }),
            AreaTarget::Agent(_)
        ));
        assert!(matches!(
            rune_target(&UseTarget {
                item: None,
                agent: None,
                position: Some(here),
            }),
            AreaTarget::Position(_)
        ));
        assert!(matches!(
            rune_target(&UseTarget {
                item: None,
                agent: None,
                position: None,
            }),
            AreaTarget::None
        ));
    }

    #[test]
    fn a_successful_rune_spends_one_from_the_stack_and_stamps_both_clocks() {
        let mut spell = a_spell(1, 0, Vec::new());
        spell.delivery = SpellDelivery::Rune;

        let (result, _events, left, tick, map, agent) = throw_rune(&spell);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(left, Some(2), "the rune was not spent");
        assert!(
            map.get_agent(agent)
                .unwrap()
                .next_spell_group_tick(SpellGroup::Attack)
                > tick
        );
    }

    /// The user-facing half of the heal filter: `exura gran mas res` must not heal the
    /// monsters around the caster, but a healing rune aimed at one must reach it.
    #[test]
    fn an_aimed_healing_rune_reaches_a_creature() {
        let mut spell = a_spell(1, 0, Vec::new());
        spell.delivery = SpellDelivery::Rune;
        spell.group = SpellGroup::Healing;
        spell.effect = SpellEffect::Healing(SpellHealing {
            target: TargetMode::Aimed,
            power: PowerCurve {
                base_power: 20.0,
                level_factor: 0.0,
                magic_factor: 0.0,
                melee_factor: 0.0,
                spread_min: 0.0,
                spread_max: 0.0,
                flat: 0.0,
            },
        });

        let mut h = TestHarness::new();
        let here = Position::new(10, 10, 7);
        let there = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        let item = a_rune(3, spell.id);
        let guid = item.guid;
        map.insert_tile(here.clone(), a_tile_with(item));
        map.insert_tile(there.clone(), MapTile::new());
        let agent = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();
        let creature = map
            .insert_agent(a_test_creature("Rat", 10, (1, 2)), &there)
            .unwrap();
        map.get_agent_mut(creature).unwrap().remove_life(9);

        let source = ItemRef {
            guid,
            placement: ItemPlacement::Map(here.clone()),
        };
        let mut map = WorldMap::new(map);
        let result = rune(
            &mut h.ctx(&mut map),
            agent,
            &source,
            &spell,
            &UseTarget {
                item: None,
                agent: Some(creature),
                position: None,
            },
        );

        assert!(result.is_ok(), "{result:?}");
        assert!(
            map.get_agent(creature).unwrap().life().current > 1,
            "the creature was not healed"
        );
        assert_eq!(map.get_top_item(&here).map(|item| item.amount), Some(2));
    }

    #[test]
    fn an_aimed_attack_rune_pointed_at_its_own_caster_is_refused() {
        let mut spell = a_spell(1, 0, Vec::new());
        spell.delivery = SpellDelivery::Rune;

        let (result, _events, left, ..) = throw_rune(&attack_aimed(spell));

        assert!(
            matches!(result, Err(ItemActionError::Reported)),
            "{result:?}"
        );
        assert_eq!(left, Some(3), "the rune was spent on nobody");
    }

    #[test]
    fn a_single_target_rune_aimed_at_bare_ground_is_refused() {
        let mut spell = a_spell(1, 0, Vec::new());
        spell.delivery = SpellDelivery::Rune;
        spell.effect = SpellEffect::Healing(SpellHealing {
            target: TargetMode::Aimed,
            power: PowerCurve {
                base_power: 1.0,
                level_factor: 0.0,
                magic_factor: 0.0,
                melee_factor: 0.0,
                spread_min: 0.0,
                spread_max: 0.0,
                flat: 0.0,
            },
        });

        let mut h = TestHarness::new();
        let here = Position::new(10, 10, 7);
        let empty = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        let item = a_rune(3, spell.id);
        let guid = item.guid;
        map.insert_tile(here.clone(), a_tile_with(item));
        map.insert_tile(empty.clone(), MapTile::new());
        let agent = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();

        let source = ItemRef {
            guid,
            placement: ItemPlacement::Map(here.clone()),
        };
        let mut map = WorldMap::new(map);
        let result = rune(
            &mut h.ctx(&mut map),
            agent,
            &source,
            &spell,
            &UseTarget {
                item: None,
                agent: None,
                position: Some(empty),
            },
        );

        assert!(
            matches!(result, Err(ItemActionError::Reported)),
            "a healing rune thrown at empty ground was accepted: {result:?}"
        );
        assert_eq!(
            map.get_top_item(&here).map(|item| item.amount),
            Some(3),
            "the rune was spent on nobody"
        );
        assert!(matches!(
            h.events.as_slice(),
            [BroadcastMessage::SpellDenied {
                reason: SpellCastingDenyReason::InvalidTarget,
                ..
            }]
        ));
    }

    #[test]
    fn a_single_target_rune_aimed_at_an_agent_is_spent() {
        let mut spell = a_spell(1, 0, Vec::new());
        spell.delivery = SpellDelivery::Rune;
        spell.effect = SpellEffect::Healing(SpellHealing {
            target: TargetMode::Aimed,
            power: PowerCurve {
                base_power: 1.0,
                level_factor: 0.0,
                magic_factor: 0.0,
                melee_factor: 0.0,
                spread_min: 0.0,
                spread_max: 0.0,
                flat: 0.0,
            },
        });

        let (result, _events, left, ..) = throw_rune(&spell);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(left, Some(2));
    }

    #[test]
    fn a_refused_rune_keeps_its_stack_and_reports_the_reason_itself() {
        let mut spell = a_spell(99, 0, Vec::new());
        spell.delivery = SpellDelivery::Rune;

        let (result, events, left, ..) = throw_rune(&spell);

        assert!(matches!(result, Err(ItemActionError::Reported)));
        assert_eq!(left, Some(3), "a refused rune was spent");
        assert!(
            matches!(
                events.as_slice(),
                [BroadcastMessage::SpellDenied {
                    reason: SpellCastingDenyReason::RequirementFailed,
                    delivery: SpellDelivery::Rune,
                    ..
                }]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_potion_restores_the_agent_it_was_used_on() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Other,
            1,
        );

        assert!(!drunk.denied);
        assert_eq!(drunk.life.current, 10, "the user was healed instead");
        assert!(
            (110..=210).contains(&drunk.target_life.current),
            "target life {}",
            drunk.target_life.current
        );
        assert_eq!(drunk.charges_left, Some(2), "the user's charge was spent");
    }

    #[test]
    fn a_potion_with_no_agent_is_denied_and_keeps_its_charge() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Nobody,
            1,
        );

        assert!(drunk.denied);
        assert_eq!(drunk.life.current, 10);
        assert_eq!(drunk.charges_left, Some(3), "a denied potion was spent");
        assert_eq!(drunk.broadcast_kinds(), ["denied"]);
    }

    #[test]
    fn drinking_announces_itself_over_the_target() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Other,
            1,
        );

        let announced = drunk.broadcasts.iter().find_map(|b| match b {
            BroadcastMessage::PotionDrunk { target, position } => Some((*target, position.clone())),
            _ => None,
        });
        let (target, position) = announced.expect("no PotionDrunk was broadcast");

        assert_ne!(
            target, drunk.user,
            "announced over the drinker, not the target"
        );
        assert_eq!(position, Position::new(10, 11, 7), "the target's tile");
    }

    #[test]
    fn a_denied_potion_announces_nothing() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Nobody,
            1,
        );

        assert!(
            !drunk
                .broadcasts
                .iter()
                .any(|b| matches!(b, BroadcastMessage::PotionDrunk { .. }))
        );
    }

    /// A creature's mana pool is 0/0, so there is nothing to restore -- but the restore is
    /// still announced at zero, because that event is what puts the effect over the tile.
    /// Silence here would be a potion that visibly did nothing.
    #[test]
    fn a_mana_potion_on_a_creature_spends_the_charge_and_restores_nothing() {
        let drunk = drink_potion_on(
            a_potion(3, None, bounds(75, 125), None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Creature,
            1,
        );

        assert!(!drunk.denied);
        assert_eq!(drunk.charges_left, Some(2));
        assert_eq!(drunk.broadcast_kinds(), ["mana"]);
        assert!(
            drunk.broadcasts.iter().any(|b| matches!(
                b,
                BroadcastMessage::AgentHealed {
                    amount: 0,
                    restore_type: RestoreType::Mana,
                    ..
                }
            )),
            "a creature has no mana to take, so the restore must report zero"
        );
    }

    #[test]
    fn a_health_potion_heals_a_creature() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Creature,
            1,
        );

        assert!(!drunk.denied);
        assert!(drunk.target_life.current > 10);
        assert_eq!(drunk.broadcast_kinds(), ["life"]);
    }

    #[test]
    fn drinking_leaves_the_empty_flask_with_the_user_not_the_target() {
        let flask = a_flask_id();
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, Some(flask)),
            pool(10, 1000),
            pool(50, 1000),
            Target::Other,
            1,
        );

        assert!(!drunk.denied);
        assert_eq!(drunk.flasks_at_the_users_feet, 1);
        assert_eq!(drunk.charges_left, Some(2));
    }

    #[test]
    fn a_potion_with_no_flask_leaves_nothing_behind() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, None),
            pool(10, 1000),
            pool(50, 1000),
            Target::Myself,
            1,
        );

        assert_eq!(drunk.flasks_at_the_users_feet, 0);
    }

    #[test]
    fn a_flask_the_catalogue_does_not_carry_does_not_stop_the_drink() {
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, Some(ItemId(65535))),
            pool(10, 1000),
            pool(50, 1000),
            Target::Myself,
            1,
        );

        assert!(!drunk.denied);
        assert!(drunk.life.current > 10, "the drink was rolled back");
        assert_eq!(drunk.charges_left, Some(2));
    }

    fn a_pouch(id: ItemId, capacity: u8) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                id,
                "pouch".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Container, ItemFlag::Take]),
                [ItemAttribute::Capacity(capacity), ItemAttribute::Weight(10)].to_vec(),
            )),
            1,
        )
    }

    struct Pouches {
        denied: bool,
        contents: Vec<Vec<(ItemId, u8)>>,
    }

    /// Drinks a potion out of a pouch inside the drinker's backpack. The backpack holds
    /// `pouches` pouches and the potion goes in the *last* one, on top of `beside_it`, so
    /// a flask handed back to "the first available container" lands in the wrong pouch
    /// and the test sees it. Reports what each pouch holds afterwards.
    fn drink_from_a_pouch(potion: Item, pouches: usize, beside_it: Vec<Item>) -> Pouches {
        let mut h = TestHarness::seeded(1);
        let here = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(here.clone(), MapTile::new());

        let potion_guid = potion.guid;
        let mut backpack = a_pouch(ItemId(1988), 20);
        let mut guids = Vec::new();
        for n in 0..pouches {
            let mut pouch = a_pouch(ItemId(1990 + n as u16), 8);
            guids.push(pouch.guid);
            if n == pouches - 1 {
                let content = pouch.content.as_mut().unwrap();
                content.extend(beside_it.iter().cloned());
                content.push(potion.clone());
            }
            backpack.content.as_mut().unwrap().push(pouch);
        }

        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.inventory = HashMap::from([(InventorySlot::Backpack, backpack)]);
        let user = map
            .insert_agent(Agent::from_player(snapshot), &here)
            .unwrap();

        let mut map = WorldMap::new(map);
        use_item_with(
            &mut h.ctx(&mut map),
            user,
            ItemRef {
                guid: potion_guid,
                placement: ItemPlacement::Container {
                    guid: *guids.last().unwrap(),
                    within: Box::new(ItemPlacement::Inventory(InventorySlot::Backpack, user)),
                    index: beside_it.len(),
                },
            },
            UseTarget {
                item: None,
                agent: Some(user),
                position: None,
            },
        );

        let backpack = map
            .get_player(user)
            .unwrap()
            .inventory()
            .get(&InventorySlot::Backpack)
            .unwrap();
        let contents = guids
            .iter()
            .map(|guid| {
                backpack
                    .find_by_guid(guid)
                    .unwrap()
                    .content
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|it| (it.id(), it.amount))
                    .collect()
            })
            .collect();

        Pouches {
            denied: h
                .events
                .iter()
                .any(|b| matches!(b, BroadcastMessage::UseItemDenied { .. })),
            contents,
        }
    }

    #[test]
    fn a_returned_flask_stacks_onto_a_like_flask_in_the_same_container() {
        let flask = a_flask_id();
        let pouches = drink_from_a_pouch(
            a_potion(3, bounds(100, 200), None, Some(flask)),
            1,
            vec![an_item(flask)],
        );

        assert!(!pouches.denied);
        assert_eq!(pouches.contents[0], vec![(flask, 2), (ItemId(9999), 2)]);
    }

    #[test]
    fn a_returned_flask_does_not_push_a_stack_past_its_maximum() {
        let flask = a_flask_id();
        let pouches = drink_from_a_pouch(
            a_potion(3, bounds(100, 200), None, Some(flask)),
            1,
            vec![Item::new(ITEM_CONFIGS[&flask].clone(), MAX_STACK_AMOUNT)],
        );

        assert!(!pouches.denied);
        assert_eq!(
            pouches.contents[0],
            vec![(flask, MAX_STACK_AMOUNT), (flask, 1), (ItemId(9999), 2)],
            "a capped stack was topped up anyway"
        );
    }

    /// Two pouches, both with room, and the potion in the second: "the first available
    /// container" would put the flask in the wrong one.
    #[test]
    fn a_returned_flask_lands_in_the_container_the_potion_came_from() {
        let flask = a_flask_id();
        let pouches =
            drink_from_a_pouch(a_potion(3, bounds(100, 200), None, Some(flask)), 2, vec![]);

        assert!(!pouches.denied);
        assert_eq!(
            pouches.contents[0],
            Vec::new(),
            "the flask went to the first pouch"
        );
        assert_eq!(pouches.contents[1], vec![(flask, 1), (ItemId(9999), 2)]);
    }

    /// The order is the assertion, not incidental: a flask returned to a tile is
    /// appended, so it sits on top of the charges still there. In a container it
    /// goes back to the potion's own slot instead -- see the pouch test above.
    #[test]
    fn a_potion_drunk_from_the_ground_leaves_the_flask_on_that_tile() {
        let flask = a_flask_id();
        let drunk = drink_potion_on(
            a_potion(3, bounds(100, 200), None, Some(flask)),
            pool(10, 1000),
            pool(50, 1000),
            Target::Myself,
            1,
        );

        assert!(!drunk.denied);
        assert_eq!(drunk.at_the_users_feet, vec![(ItemId(9999), 2), (flask, 1)]);
    }
}
