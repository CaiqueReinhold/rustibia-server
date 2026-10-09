use std::sync::{Arc, Mutex};

use minijinja::{Value, context};
use tracing::error;

use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::{Agent, AgentKey},
        items::ItemId,
        npc::{Keyword, NpcResponse},
        position::Position,
    },
    game::{
        TickCtx,
        config::GAME_CONFIG,
        events::BroadcastMessage,
        map_query::face_target,
        money::{COINS, pay, plan_item_payment, plan_payment},
        random::Rolls,
    },
};

enum Outcome {
    Ignored,
    Answered,
    Ended,
}

pub fn respond_player(ctx: &mut TickCtx, player_key: AgentKey, npc_key: AgentKey, message: &str) {
    let rolls = Mutex::new(Rolls::new(ctx.roll.uniform(0, u32::MAX).into()));
    let Some(npc) = ctx.map.get_agent(npc_key) else {
        return;
    };
    let Some(npc_def) = npc.get_npc() else {
        return;
    };
    let engaged = match npc.target() {
        Some(target) if target != player_key => return,
        Some(_) => true,
        None => false,
    };
    let Some(player) = ctx.map.get_agent(player_key) else {
        return;
    };
    let player_name = player.name();
    let Some(inventory) = player.get_player().map(|player| player.inventory().clone()) else {
        return;
    };

    let uniform =
        Value::from_function(move |min: u32, max: u32| rolls.lock().unwrap().uniform(min, max));
    let has_item =
        Value::from_function(move |id: u16| plan_item_payment(&inventory, ItemId(id)).is_some());
    let branch_parent = npc.keyword_branch().map(|(parent, _)| parent.clone());
    let context = |word: &str, from_branch: bool| {
        context! {
            PLAYERNAME => player_name,
            WORD => word,
            PARENT_KEYWORD => from_branch.then(|| branch_parent.clone()).flatten(),
            uniform => uniform.clone(),
            has_item => has_item.clone(),
            ..Value::from_serialize(&npc_def.arguments)
        }
    };

    let allows = |keyword: &Keyword, word: &str, from_branch: bool| {
        keyword
            .allows(&npc_def.environment, &context(word, from_branch))
            .unwrap_or_else(|e| {
                error!("NPC {} could not check {word:?}: {e}", npc_def.name);
                false
            })
    };
    let Some((word, keyword, from_branch)) = match_keywords(npc, message, allows) else {
        return;
    };
    let response = match keyword.response(&npc_def.environment, context(&word, from_branch)) {
        Ok(response) => response,
        Err(e) => {
            error!("NPC {} could not answer {word:?}: {e}", npc_def.name);
            return;
        }
    };
    if !engaged && !matches!(response, NpcResponse::Engage(_)) {
        return;
    }

    let outcome = match response {
        NpcResponse::Engage(text) => engage_player(ctx, player_key, npc_key, text),
        NpcResponse::Dismiss(text) => dismiss_player(ctx, player_key, npc_key, text),
        NpcResponse::Say(text) => say(ctx, player_key, npc_key, text),
        NpcResponse::Teleport {
            cost,
            item,
            to,
            failed_text,
        } => teleport_player(ctx, player_key, npc_key, failed_text, cost, item, to),
    };

    let branch = match outcome {
        Outcome::Ignored => return,
        Outcome::Ended => None,
        Outcome::Answered if keyword.resets() => None,
        Outcome::Answered if !keyword.children().is_empty() => Some((word, keyword)),
        Outcome::Answered if from_branch => return,
        Outcome::Answered => None,
    };
    if let Some(mut npc) = ctx.map.agent_mut(npc_key) {
        npc.set_keyword_branch(branch);
    }
}

/// The branch's children first, then the root. The flag says which of the two matched.
fn match_keywords(
    npc: &Agent,
    message: &str,
    allows: impl Fn(&Keyword, &str, bool) -> bool,
) -> Option<(String, Arc<Keyword>, bool)> {
    let npc_def = npc.get_npc()?;
    let branch = npc
        .keyword_branch()
        .map(|(_, keyword)| keyword.children())
        .unwrap_or_default();

    let matching = |keywords: &[Arc<Keyword>], from_branch: bool| {
        keywords.iter().find_map(|keyword| {
            keyword
                .match_word(message)
                .filter(|word| allows(keyword, word, from_branch))
                .map(|word| (word, Arc::clone(keyword), from_branch))
        })
    };
    matching(branch, true).or_else(|| matching(npc_def.keywords.as_slice(), false))
}

fn engage_player(
    ctx: &mut TickCtx,
    player_key: AgentKey,
    npc_key: AgentKey,
    text: String,
) -> Outcome {
    if ctx
        .map
        .get_agent(npc_key)
        .is_some_and(|a| a.target().is_some())
    {
        return Outcome::Ignored;
    }
    if let Some(mut npc) = ctx.map.agent_mut(npc_key) {
        npc.set_target(Some(player_key), 0);
    }
    face_player(ctx, npc_key, player_key);
    npc_say(ctx, npc_key, text);
    Outcome::Ended
}

fn dismiss_player(
    ctx: &mut TickCtx,
    player_key: AgentKey,
    npc_key: AgentKey,
    text: String,
) -> Outcome {
    if let Some(mut npc) = ctx.map.agent_mut(npc_key) {
        npc.set_target(None, 0);
    }
    face_player(ctx, npc_key, player_key);
    npc_say(ctx, npc_key, text);
    Outcome::Ended
}

fn say(ctx: &mut TickCtx, player_key: AgentKey, npc_key: AgentKey, text: String) -> Outcome {
    face_player(ctx, npc_key, player_key);
    npc_say(ctx, npc_key, text);
    Outcome::Answered
}

fn teleport_player(
    ctx: &mut TickCtx,
    player_key: AgentKey,
    npc_key: AgentKey,
    failed_text: String,
    cost: u64,
    item: Option<ItemId>,
    to: Position,
) -> Outcome {
    let Some(from) = ctx.map.agent_position(player_key).cloned() else {
        return Outcome::Ignored;
    };
    let payment = ctx.map.get_player(player_key).and_then(|player| {
        let coins = plan_payment(player.inventory(), &COINS, cost)?;
        match item {
            Some(item) => Some(coins.and(plan_item_payment(player.inventory(), item)?)),
            None => Some(coins),
        }
    });
    let Some(payment) = payment else {
        face_player(ctx, npc_key, player_key);
        npc_say(ctx, npc_key, failed_text);
        return Outcome::Answered;
    };

    if let Err(e) = ctx.map.move_agent(player_key, &to) {
        error!("NPC teleport to {to:?} failed: {e:?}");
        return Outcome::Ignored;
    }
    pay(ctx, player_key, payment, &COINS);

    if let Some(mut npc) = ctx.map.agent_mut(npc_key) {
        npc.set_target(None, 0);
    }
    ctx.events.push(BroadcastMessage::AgentTeleported {
        agent_key: player_key,
        from_position: from,
        to_position: to,
    });
    Outcome::Ended
}

fn face_player(ctx: &mut TickCtx, npc_key: AgentKey, player_key: AgentKey) {
    let (Some(npc_pos), Some(player_pos)) = (
        ctx.map.agent_position(npc_key),
        ctx.map.agent_position(player_key),
    ) else {
        return;
    };
    let facing = face_target(npc_pos, player_pos);
    if let Some(mut npc) = ctx.map.agent_mut(npc_key) {
        npc.set_facing(facing);
    }
}

fn npc_say(ctx: &mut TickCtx, npc_key: AgentKey, text: String) {
    ctx.scheduled.push(ScheduledCommand {
        at_tick: ctx.tick + GAME_CONFIG.chat.npc_reply_delay_ticks,
        command: WorldCommand::Say {
            agent_key: npc_key,
            message: text,
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::world::WorldCommand;
    use crate::entities::{
        inventory::InventorySlot,
        items::{Item, ItemConfig, ItemFlag},
        map::{GameMap, MapTile},
        world_map::WorldMap,
    };
    use crate::game::{TestHarness, config::GAME_CONFIG, money::COINS};
    use crate::persistence::test_fixtures::{a_test_snapshot, an_npc};
    use std::collections::HashMap;

    const HERE: Position = Position { x: 10, y: 10, z: 7 };
    const EDRON: Position = Position { x: 20, y: 20, z: 7 };

    struct World {
        map: WorldMap,
        h: TestHarness,
        player: AgentKey,
        npc: AgentKey,
    }

    impl World {
        fn new(gold: u8, edron_exists: bool) -> Self {
            Self::carrying(gold, edron_exists, [])
        }

        fn with_ring(gold: u8) -> Self {
            let ring = ItemConfig::new(
                ItemId(9999),
                "ring".to_string(),
                None,
                None,
                [ItemFlag::Take],
                [],
            );
            Self::carrying(
                gold,
                true,
                [(InventorySlot::Ring, Item::new(Arc::new(ring), 1))],
            )
        }

        fn carrying(
            gold: u8,
            edron_exists: bool,
            items: impl IntoIterator<Item = (InventorySlot, Item)>,
        ) -> Self {
            let mut map = GameMap::new();
            map.insert_tile(HERE, MapTile::new());
            if edron_exists {
                map.insert_tile(EDRON, MapTile::new());
            }
            let mut snapshot = a_test_snapshot(1, 1);
            snapshot.inventory =
                HashMap::from([(InventorySlot::RightHand, Item::new(COINS.cheapest(), gold))]);
            snapshot.inventory.extend(items);
            let player = map
                .insert_agent(Agent::from_player(snapshot), &HERE)
                .unwrap();
            let npc = map.insert_agent(Agent::from_npc(an_npc()), &HERE).unwrap();
            World {
                map: WorldMap::new(map),
                h: TestHarness::new(),
                player,
                npc,
            }
        }

        fn another_player(&mut self) -> AgentKey {
            self.map
                .insert_agent(Agent::from_player(a_test_snapshot(2, 2)), &HERE)
                .unwrap()
        }

        fn hear(&mut self, player: AgentKey, message: &str) {
            respond_player(&mut self.h.ctx(&mut self.map), player, self.npc, message);
        }

        fn said(&self) -> Vec<&str> {
            self.h
                .scheduled
                .iter()
                .filter_map(|scheduled| match &scheduled.command {
                    WorldCommand::Say { message, .. } => Some(message.as_str()),
                    _ => None,
                })
                .collect()
        }

        fn branch(&self) -> Option<&str> {
            self.map
                .get_agent(self.npc)
                .unwrap()
                .keyword_branch()
                .map(|(word, _)| word.as_str())
        }

        fn has_ring(&self) -> bool {
            self.map
                .get_player(self.player)
                .unwrap()
                .inventory()
                .get(&InventorySlot::Ring)
                .is_some()
        }

        fn gold(&self) -> u8 {
            self.map
                .get_player(self.player)
                .unwrap()
                .inventory()
                .get(&InventorySlot::RightHand)
                .map_or(0, |coins| coins.amount)
        }
    }

    #[test]
    fn a_free_npc_answers_only_a_greeting() {
        let mut w = World::new(100, true);

        w.hear(w.player, "sail");
        w.hear(w.player, "hi");

        assert_eq!(w.said(), ["Welcome, Rizael."]);
        assert_eq!(w.map.get_agent(w.npc).unwrap().target(), Some(w.player));
    }

    #[test]
    fn an_npc_talking_to_someone_else_ignores_the_message() {
        let mut w = World::new(100, true);
        let other = w.another_player();
        w.hear(other, "hi");

        w.hear(w.player, "edron");

        assert_eq!(w.said().len(), 1);
        assert_eq!(w.branch(), None);
    }

    #[test]
    fn a_branch_word_matches_before_a_root_word() {
        let mut w = World::new(100, true);
        w.hear(w.player, "hi");
        w.hear(w.player, "edron");

        w.hear(w.player, "no");

        assert_eq!(w.said().last(), Some(&"Maybe later."));
    }

    #[test]
    fn a_root_word_drops_the_branch() {
        let mut w = World::new(100, true);
        w.hear(w.player, "hi");
        w.hear(w.player, "edron");
        assert_eq!(w.branch(), Some("edron"));

        w.hear(w.player, "sail");

        assert_eq!(w.said().last(), Some(&"Where to?"));
        assert_eq!(w.branch(), None);
    }

    #[test]
    fn a_reset_keyword_drops_the_branch() {
        let mut w = World::new(100, true);
        w.hear(w.player, "hi");
        w.hear(w.player, "edron");

        w.hear(w.player, "no");

        assert_eq!(w.branch(), None);
    }

    #[test]
    fn a_goodbye_ends_the_conversation_and_the_branch() {
        let mut w = World::new(100, true);
        w.hear(w.player, "hi");
        w.hear(w.player, "edron");

        w.hear(w.player, "bye");

        assert_eq!(w.branch(), None);
        assert_eq!(w.map.get_agent(w.npc).unwrap().target(), None);
    }

    #[test]
    fn a_teleport_moves_the_player_and_charges_the_cost() {
        let mut w = World::new(100, true);
        w.hear(w.player, "hi");
        w.hear(w.player, "edron");

        w.hear(w.player, "yes");

        assert_eq!(w.map.agent_position(w.player), Some(&EDRON));
        assert_eq!(w.gold(), 60);
        assert_eq!(w.branch(), None);
    }

    #[test]
    fn a_player_who_cannot_pay_hears_the_failed_text_and_stays() {
        let mut w = World::new(39, true);
        w.hear(w.player, "hi");
        w.hear(w.player, "edron");

        w.hear(w.player, "yes");

        assert_eq!(w.said().last(), Some(&"No gold, Rizael."));
        assert_eq!(w.map.agent_position(w.player), Some(&HERE));
        assert_eq!(w.gold(), 39);
        assert_eq!(w.branch(), Some("edron"));
    }

    #[test]
    fn a_teleport_to_a_missing_tile_charges_nothing() {
        let mut w = World::new(100, false);
        w.hear(w.player, "hi");
        w.hear(w.player, "edron");

        w.hear(w.player, "yes");

        assert_eq!(w.gold(), 100);
        assert_eq!(w.map.agent_position(w.player), Some(&HERE));
    }

    #[test]
    fn uniform_rolls_within_its_bounds() {
        let mut w = World::new(100, true);
        w.hear(w.player, "hi");

        for _ in 0..30 {
            w.hear(w.player, "dice");
        }

        let rolled: Vec<&str> = w.said()[1..].to_vec();
        assert!(rolled.iter().all(|roll| ["1", "2", "3"].contains(roll)));
        assert!(rolled.iter().any(|roll| *roll != rolled[0]));
    }

    #[test]
    fn a_keyword_whose_condition_fails_gives_way_to_the_next_one_with_the_word() {
        let mut w = World::new(100, true);
        w.hear(w.player, "hi");

        w.hear(w.player, "ferry");

        assert_eq!(w.said().last(), Some(&"No ring, no ferry."));
    }

    #[test]
    fn a_teleport_priced_in_an_item_takes_it_and_no_gold() {
        let mut w = World::with_ring(100);
        w.hear(w.player, "hi");
        w.hear(w.player, "ferry");

        w.hear(w.player, "yes");

        assert_eq!(w.map.agent_position(w.player), Some(&EDRON));
        assert!(!w.has_ring());
        assert_eq!(w.gold(), 100);
    }

    #[test]
    fn a_player_who_no_longer_has_the_item_hears_the_failed_text_and_stays() {
        let mut w = World::with_ring(100);
        w.hear(w.player, "hi");
        w.hear(w.player, "ferry");
        w.map
            .player_mut(w.player)
            .unwrap()
            .inventory_mut()
            .take_slot(&InventorySlot::Ring);

        w.hear(w.player, "yes");

        assert_eq!(w.said().last(), Some(&"No ring."));
        assert_eq!(w.map.agent_position(w.player), Some(&HERE));
    }

    #[test]
    fn a_reply_is_scheduled_after_the_delay_rather_than_said_at_once() {
        let mut w = World::new(100, true);

        w.hear(w.player, "hi");

        assert!(w.h.events.is_empty());
        assert_eq!(
            w.h.scheduled[0].at_tick,
            w.h.tick + GAME_CONFIG.chat.npc_reply_delay_ticks
        );
    }
}
