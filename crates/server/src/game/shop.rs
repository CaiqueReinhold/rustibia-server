use std::sync::Arc;

use tracing::error;

use crate::{
    constants::items::MAX_STACK_AMOUNT,
    entities::{
        agent::AgentKey,
        inventory::{Inventory, InventorySlot},
        items::{FluidType, Item, ItemFlag, ItemId},
        map::GameMap,
        npc::{Npc, ShopOffer},
        player::Player,
    },
    game::{
        TickCtx,
        events::BroadcastMessage,
        item_movement::stow_item,
        money::{COINS, Coins, Payment, coins_held, give_change, pay, plan_payment_with_bank},
    },
};

const TOO_FAR: &str = "You are too far away.";
const NOT_TRADED: &str = "You cannot trade this item.";
const NO_MONEY: &str = "You do not have enough money.";
const NO_CAPACITY: &str = "You do not have enough capacity.";
const NO_ROOM: &str = "You do not have enough room.";
const NOT_CARRIED: &str = "You do not have this item.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trade {
    Buy,
    Sell,
}

enum Refusal {
    Closed,
    Stale,
    Denied(&'static str),
}

pub fn close_shop(ctx: &mut TickCtx, player_key: AgentKey) {
    if let Some(mut player) = ctx.map.player_mut(player_key) {
        player.set_shop(None);
    }
}

pub fn trade(
    ctx: &mut TickCtx,
    player_key: AgentKey,
    trade: Trade,
    id: ItemId,
    subtype: u8,
    amount: u8,
) {
    let (offer, price) = match open_offer(ctx.map, player_key, trade, id, subtype, amount) {
        Ok(found) => found,
        Err(Refusal::Closed) => return,
        Err(Refusal::Stale) => return close_shop(ctx, player_key),
        Err(Refusal::Denied(message)) => return deny(ctx, player_key, message),
    };
    let traded = match trade {
        Trade::Buy => buy(ctx, player_key, &offer, price, amount),
        Trade::Sell => sell(ctx, player_key, &offer, price, amount),
    };
    if let Err(message) = traded {
        deny(ctx, player_key, message);
    }
}

/// The offer and its unit price in the direction asked, once the shop is still open, its NPC
/// still talking to the player and in range.
fn open_offer(
    map: &GameMap,
    player_key: AgentKey,
    trade: Trade,
    id: ItemId,
    subtype: u8,
    amount: u8,
) -> Result<(ShopOffer, u64), Refusal> {
    let npc_key = map
        .get_player(player_key)
        .and_then(Player::shop)
        .ok_or(Refusal::Closed)?;
    let npc = map
        .get_agent(npc_key)
        .filter(|npc| npc.target() == Some(player_key))
        .and_then(|npc| npc.get_npc())
        .ok_or(Refusal::Stale)?;
    let (Some(at), Some(npc_at)) = (map.agent_position(player_key), map.agent_position(npc_key))
    else {
        return Err(Refusal::Stale);
    };
    if !at.is_within(npc_at, npc.talk_range) {
        return Err(Refusal::Denied(TOO_FAR));
    }
    let offer = npc.offer(id, subtype).ok_or(Refusal::Denied(NOT_TRADED))?;
    let price = match trade {
        Trade::Buy => offer.buy,
        Trade::Sell => offer.sell,
    };
    match price {
        Some(price) if (1..=MAX_STACK_AMOUNT).contains(&amount) => {
            Ok((offer.clone(), price as u64))
        }
        _ => Err(Refusal::Denied(NOT_TRADED)),
    }
}

fn buy(
    ctx: &mut TickCtx,
    player_key: AgentKey,
    offer: &ShopOffer,
    price: u64,
    amount: u8,
) -> Result<(), &'static str> {
    let Some(player) = ctx.map.get_player(player_key) else {
        return Ok(());
    };
    let (payment, from_bank) = plan_payment_with_bank(
        player.inventory(),
        &COINS,
        player.bank_balance(),
        price * amount as u64,
    )
    .ok_or(NO_MONEY)?;
    if offer.config.attr_weight().unwrap_or(0) * amount as u32 > player.capacity_available() {
        return Err(NO_CAPACITY);
    }
    let items = bought_items(offer, amount);
    let change_stacks = COINS.make_change(payment.change()).len();
    if free_container_slots(player.inventory()) < items.len() + change_stacks {
        return Err(NO_ROOM);
    }

    for item in items {
        if let Err(e) = stow_item(ctx, player_key, item) {
            error!("a purchase planned for {player_key:?} found no room: {e}");
            return Ok(());
        }
    }
    pay(ctx, player_key, payment, &COINS);
    if from_bank > 0
        && let Some(mut player) = ctx.map.player_mut(player_key)
    {
        player.withdraw(from_bank);
    }
    Ok(())
}

fn bought_items(offer: &ShopOffer, amount: u8) -> Vec<Item> {
    let config = &offer.config;
    if config.has_flag(ItemFlag::Cumulative) {
        vec![Item::new(Arc::clone(config), amount)]
    } else {
        (0..amount)
            .map(|_| {
                let mut item = Item::new(Arc::clone(config), 1);
                item.fluid = offer.fluid;
                item
            })
            .collect()
    }
}

fn sell(
    ctx: &mut TickCtx,
    player_key: AgentKey,
    offer: &ShopOffer,
    price: u64,
    amount: u8,
) -> Result<(), &'static str> {
    let Some(player) = ctx.map.get_player(player_key) else {
        return Ok(());
    };
    let sale = plan_sale(player.inventory(), offer.id(), offer.fluid, amount).ok_or(NOT_CARRIED)?;
    pay(ctx, player_key, sale, &COINS);
    for coin in COINS.make_change(price * amount as u64) {
        give_change(ctx, player_key, coin);
    }
    Ok(())
}

fn deny(ctx: &mut TickCtx, player_key: AgentKey, message: &str) {
    ctx.events.push(BroadcastMessage::TradeDenied {
        agent_key: player_key,
        message: message.to_owned(),
    });
}

pub fn money(player: &Player, coins: &Coins) -> u64 {
    coins_held(player.inventory(), coins) + player.bank_balance()
}

pub fn goods(npc: &Npc, inventory: &Inventory) -> Vec<(ItemId, u8, u16)> {
    npc.shop
        .iter()
        .filter(|offer| offer.sell.is_some())
        .filter_map(|offer| {
            let count = carried_count(inventory, offer.id(), offer.fluid);
            (count > 0).then(|| {
                (
                    offer.id(),
                    offer.subtype(),
                    count.min(u16::MAX as u32) as u16,
                )
            })
        })
        .collect()
}

pub fn carried_count(inventory: &Inventory, id: ItemId, fluid: Option<FluidType>) -> u32 {
    sellable(inventory, id, fluid)
        .map(|item| item.amount as u32)
        .sum()
}

pub fn plan_sale(
    inventory: &Inventory,
    id: ItemId,
    fluid: Option<FluidType>,
    amount: u8,
) -> Option<Payment> {
    let mut takes = Vec::new();
    let mut remaining = amount;
    for item in sellable(inventory, id, fluid) {
        if remaining == 0 {
            break;
        }
        let taken = remaining.min(item.amount);
        takes.push((InventorySlot::Backpack, item.guid, taken));
        remaining -= taken;
    }
    (remaining == 0).then(|| Payment::taking(takes))
}

pub fn free_container_slots(inventory: &Inventory) -> usize {
    inventory
        .get(&InventorySlot::Backpack)
        .map_or(0, free_slots)
}

fn free_slots(item: &Item) -> usize {
    item.available_capacity().unwrap_or(0) + contents(item).map(free_slots).sum::<usize>()
}

/// What the backpack slot's container holds, at any depth, that can be sold as `id` holding
/// `fluid`: a container only when empty.
fn sellable(
    inventory: &Inventory,
    id: ItemId,
    fluid: Option<FluidType>,
) -> impl Iterator<Item = &Item> {
    let mut carried = Vec::new();
    if let Some(backpack) = inventory.get(&InventorySlot::Backpack) {
        collect(backpack, &mut carried);
    }
    carried.into_iter().filter(move |item| {
        item.id() == id
            && item.fluid == fluid
            && item.content().is_none_or(|content| content.is_empty())
    })
}

fn collect<'a>(container: &'a Item, found: &mut Vec<&'a Item>) {
    for item in contents(container) {
        found.push(item);
        collect(item, found);
    }
}

fn contents(item: &Item) -> impl Iterator<Item = &Item> {
    item.content().into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::entities::{
        inventory::Wearer,
        items::{ItemAttribute, ItemConfig, ItemFlag},
        vocation::Vocation,
    };
    use crate::persistence::test_fixtures::{A_TORCH, A_VIAL, AN_ARROW, a_shop_catalogue, an_npc};

    fn a(id: ItemId, amount: u8) -> Item {
        Item::new(Arc::clone(&a_shop_catalogue()[&id]), amount)
    }

    fn a_bag(capacity: u8, content: Vec<Item>) -> Item {
        let mut bag = Item::new(
            Arc::new(ItemConfig::new(
                ItemId(2853),
                "bag".to_string(),
                None,
                None,
                [ItemFlag::Container],
                [ItemAttribute::Capacity(capacity)],
            )),
            1,
        );
        bag.set_content(content);
        bag
    }

    fn an_inventory(slots: Vec<(InventorySlot, Item)>) -> Inventory {
        Inventory::from_snapshot(
            HashMap::from_iter(slots),
            Wearer {
                level: 1,
                vocation: Vocation::Knight,
            },
        )
    }

    #[test]
    fn carried_items_are_counted_at_any_depth_and_stacks_are_summed() {
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(
                20,
                vec![
                    a(AN_ARROW, 30),
                    a_bag(8, vec![a(AN_ARROW, 12), a(A_TORCH, 1)]),
                ],
            ),
        )]);

        assert_eq!(carried_count(&inventory, AN_ARROW, None), 42);
        assert_eq!(carried_count(&inventory, A_TORCH, None), 1);
    }

    #[test]
    fn an_equipped_item_is_not_carried() {
        let inventory = an_inventory(vec![
            (InventorySlot::RightHand, a(A_TORCH, 1)),
            (InventorySlot::Backpack, a_bag(20, vec![])),
        ]);

        assert_eq!(carried_count(&inventory, A_TORCH, None), 0);
    }

    #[test]
    fn a_container_counts_only_when_empty() {
        let bag_id = ItemId(2853);
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(20, vec![a_bag(8, vec![]), a_bag(8, vec![a(A_TORCH, 1)])]),
        )]);

        assert_eq!(carried_count(&inventory, bag_id, None), 1);
    }

    #[test]
    fn free_slots_add_up_across_nested_containers() {
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(3, vec![a(A_TORCH, 1), a_bag(8, vec![a(A_TORCH, 1)])]),
        )]);

        assert_eq!(free_container_slots(&inventory), 1 + 7);
    }

    #[test]
    fn without_a_backpack_there_is_no_room() {
        assert_eq!(free_container_slots(&an_inventory(vec![])), 0);
    }

    #[test]
    fn a_sale_takes_from_several_stacks_and_splits_the_last() {
        let first = a(AN_ARROW, 30);
        let second = a(AN_ARROW, 30);
        let (first_guid, second_guid) = (first.guid, second.guid);
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(20, vec![first, second]),
        )]);

        assert_eq!(
            plan_sale(&inventory, AN_ARROW, None, 40),
            Some(Payment::taking(vec![
                (InventorySlot::Backpack, first_guid, 30),
                (InventorySlot::Backpack, second_guid, 10),
            ]))
        );
    }

    #[test]
    fn a_sale_of_more_than_is_carried_plans_nothing() {
        let inventory = an_inventory(vec![
            (InventorySlot::RightHand, a(A_TORCH, 1)),
            (InventorySlot::Backpack, a_bag(20, vec![a(A_TORCH, 1)])),
        ]);

        assert_eq!(plan_sale(&inventory, A_TORCH, None, 2), None);
    }

    #[test]
    fn goods_list_only_sellable_offers_the_player_carries() {
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(20, vec![a(A_TORCH, 1), a(A_TORCH, 1), a(AN_ARROW, 5)]),
        )]);

        assert_eq!(goods(&an_npc(), &inventory), vec![(A_TORCH, 0, 2)]);
    }

    use crate::entities::{
        agent::{Agent, AgentKey},
        map::{GameMap, MapTile},
        position::Position,
        world_map::WorldMap,
    };
    use crate::game::{TestHarness, events::BroadcastMessage, money::COINS};
    use crate::persistence::test_fixtures::a_test_snapshot;

    const HERE: Position = Position { x: 10, y: 10, z: 7 };
    const FAR: Position = Position { x: 30, y: 10, z: 7 };

    struct Shop {
        map: WorldMap,
        h: TestHarness,
        player: AgentKey,
        npc: AgentKey,
    }

    impl Shop {
        /// A player at the fixture NPC's shop with `content` in a backpack of `capacity` slots.
        fn open(capacity: u8, content: Vec<Item>) -> Self {
            let mut map = GameMap::new();
            map.insert_tile(HERE, MapTile::new());
            map.insert_tile(FAR, MapTile::new());
            let mut snapshot = a_test_snapshot(1, 1);
            snapshot.inventory =
                HashMap::from([(InventorySlot::Backpack, a_bag(capacity, content))]);
            let player = map
                .insert_agent(Agent::from_player(snapshot), &HERE)
                .unwrap();
            let npc = map.insert_agent(Agent::from_npc(an_npc()), &HERE).unwrap();
            let mut map = WorldMap::new(map);
            map.agent_mut(npc).unwrap().set_target(Some(player), 0);
            map.player_mut(player).unwrap().set_shop(Some(npc));
            Shop {
                map,
                h: TestHarness::new(),
                player,
                npc,
            }
        }

        fn trade(&mut self, trade: Trade, id: ItemId, amount: u8) {
            self.trade_as(trade, id, 0, amount);
        }

        fn trade_as(&mut self, trade: Trade, id: ItemId, subtype: u8, amount: u8) {
            super::trade(
                &mut self.h.ctx(&mut self.map),
                self.player,
                trade,
                id,
                subtype,
                amount,
            );
        }

        fn inventory(&self) -> &Inventory {
            self.map.get_player(self.player).unwrap().inventory()
        }

        fn gold(&self) -> u64 {
            coins_held(self.inventory(), &COINS)
        }

        fn balance(&self) -> u64 {
            self.map.get_player(self.player).unwrap().bank_balance()
        }

        fn denied(&self) -> Vec<&str> {
            self.h
                .events
                .iter()
                .filter_map(|event| match event {
                    BroadcastMessage::TradeDenied { message, .. } => Some(message.as_str()),
                    _ => None,
                })
                .collect()
        }

        fn shop(&self) -> Option<AgentKey> {
            self.map.get_player(self.player).unwrap().shop()
        }
    }

    fn gold(amount: u8) -> Item {
        Item::new(COINS.cheapest(), amount)
    }

    #[test]
    fn a_purchase_stows_the_items_and_charges_the_price() {
        let mut s = Shop::open(20, vec![gold(100)]);

        s.trade(Trade::Buy, A_TORCH, 2);

        assert_eq!(carried_count(s.inventory(), A_TORCH, None), 2);
        assert_eq!(s.gold(), 0);
        assert!(s.denied().is_empty());
    }

    #[test]
    fn a_stackable_purchase_arrives_as_one_stack() {
        let mut s = Shop::open(20, vec![gold(100)]);

        s.trade(Trade::Buy, AN_ARROW, 10);

        assert_eq!(sellable(s.inventory(), AN_ARROW, None).count(), 1);
        assert_eq!(carried_count(s.inventory(), AN_ARROW, None), 10);
        assert_eq!(s.gold(), 70);
    }

    #[test]
    fn a_purchase_the_coins_cannot_cover_draws_the_rest_from_the_bank() {
        let mut s = Shop::open(20, vec![gold(20)]);
        s.map.player_mut(s.player).unwrap().deposit(100);

        s.trade(Trade::Buy, A_TORCH, 1);

        assert_eq!(carried_count(s.inventory(), A_TORCH, None), 1);
        assert_eq!((s.gold(), s.balance()), (0, 70));
    }

    #[test]
    fn too_little_money_charges_nothing() {
        let mut s = Shop::open(20, vec![gold(20)]);

        s.trade(Trade::Buy, A_TORCH, 1);

        assert_eq!(s.denied(), ["You do not have enough money."]);
        assert_eq!(
            (s.gold(), carried_count(s.inventory(), A_TORCH, None)),
            (20, 0)
        );
    }

    #[test]
    fn too_little_capacity_charges_nothing() {
        let mut s = Shop::open(20, vec![gold(100)]);
        let carried = s.inventory().carried_weight();
        s.map
            .player_mut(s.player)
            .unwrap()
            .set_capacity(carried + 499);

        s.trade(Trade::Buy, A_TORCH, 1);

        assert_eq!(s.denied(), ["You do not have enough capacity."]);
        assert_eq!(s.gold(), 100);
    }

    #[test]
    fn a_purchase_with_nowhere_to_go_charges_nothing() {
        let mut s = Shop::open(1, vec![gold(100)]);

        s.trade(Trade::Buy, A_TORCH, 1);

        assert_eq!(s.denied(), ["You do not have enough room."]);
        assert_eq!(s.gold(), 100);
    }

    #[test]
    fn room_for_the_change_is_needed_too() {
        let platinum = COINS.make_change(100).remove(0);
        let mut s = Shop::open(2, vec![platinum]);

        s.trade(Trade::Buy, A_TORCH, 1);

        assert_eq!(s.denied(), ["You do not have enough room."]);
        assert_eq!(s.gold(), 100);
    }

    #[test]
    fn a_player_out_of_range_is_refused_and_keeps_the_shop() {
        let mut s = Shop::open(20, vec![gold(100)]);
        s.map.move_agent(s.player, &FAR).unwrap();

        s.trade(Trade::Buy, A_TORCH, 1);

        assert_eq!(s.denied(), ["You are too far away."]);
        assert_eq!(s.shop(), Some(s.npc));
        assert_eq!(s.gold(), 100);
    }

    #[test]
    fn an_item_not_offered_that_way_or_a_bad_amount_is_refused() {
        let mut s = Shop::open(20, vec![gold(100), a(AN_ARROW, 5)]);

        s.trade(Trade::Sell, AN_ARROW, 1);
        s.trade(Trade::Buy, A_TORCH, 0);
        s.trade(Trade::Buy, A_TORCH, 101);
        s.trade(Trade::Buy, ItemId(1), 1);

        assert_eq!(s.denied(), ["You cannot trade this item."; 4]);
        assert_eq!(s.gold(), 100);
    }

    #[test]
    fn a_shop_whose_npc_has_moved_on_closes_without_a_word() {
        let mut s = Shop::open(20, vec![gold(100)]);
        s.map.agent_mut(s.npc).unwrap().set_target(None, 0);

        s.trade(Trade::Buy, A_TORCH, 1);

        assert_eq!(s.shop(), None);
        assert!(s.denied().is_empty());
        assert_eq!(s.gold(), 100);
    }

    #[test]
    fn a_sale_takes_the_items_and_pays_in_coins() {
        let mut s = Shop::open(20, vec![a(A_TORCH, 1), a(A_TORCH, 1), a(A_TORCH, 1)]);

        s.trade(Trade::Sell, A_TORCH, 2);

        assert_eq!(carried_count(s.inventory(), A_TORCH, None), 1);
        assert_eq!(s.gold(), 20);
    }

    #[test]
    fn selling_more_than_is_carried_takes_nothing() {
        let mut s = Shop::open(20, vec![a(A_TORCH, 1)]);

        s.trade(Trade::Sell, A_TORCH, 2);

        assert_eq!(s.denied(), ["You do not have this item."]);
        assert_eq!(carried_count(s.inventory(), A_TORCH, None), 1);
    }

    #[test]
    fn closing_the_shop_forgets_it() {
        let mut s = Shop::open(20, vec![]);

        close_shop(&mut s.h.ctx(&mut s.map), s.player);

        assert_eq!(s.shop(), None);
    }

    #[test]
    fn no_shipped_shop_offers_a_coin() {
        let money = &crate::game::config::GAME_CONFIG.money;
        let coins = [
            money.gold_coin_id,
            money.platinum_coin_id,
            money.crystal_coin_id,
        ];
        for npc in crate::persistence::npcs::NPCS.values() {
            for offer in &npc.shop {
                assert!(
                    !coins.contains(&offer.id()),
                    "{} offers coin {:?}",
                    npc.name,
                    offer.id()
                );
            }
        }
    }

    fn a_vial(fluid: FluidType) -> Item {
        let mut vial = a(A_VIAL, 1);
        vial.fluid = Some(fluid);
        vial
    }

    #[test]
    fn a_bought_container_holds_the_offers_fluid() {
        let mut s = Shop::open(20, vec![gold(100)]);

        s.trade_as(Trade::Buy, A_VIAL, FluidType::Milk as u8, 2);

        assert_eq!(
            carried_count(s.inventory(), A_VIAL, Some(FluidType::Milk)),
            2
        );
        assert_eq!(s.gold(), 84);
    }

    #[test]
    fn a_sale_takes_only_containers_holding_the_offers_fluid() {
        let mut s = Shop::open(20, vec![a_vial(FluidType::Milk), a_vial(FluidType::None)]);

        s.trade_as(Trade::Sell, A_VIAL, 0, 2);
        s.trade_as(Trade::Sell, A_VIAL, 0, 1);

        assert_eq!(s.denied(), [NOT_CARRIED]);
        assert_eq!(
            carried_count(s.inventory(), A_VIAL, Some(FluidType::None)),
            0
        );
        assert_eq!(
            carried_count(s.inventory(), A_VIAL, Some(FluidType::Milk)),
            1
        );
        assert_eq!(s.gold(), 5);
    }

    #[test]
    fn goods_are_counted_per_fluid() {
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(
                20,
                vec![
                    a_vial(FluidType::Milk),
                    a_vial(FluidType::None),
                    a_vial(FluidType::None),
                ],
            ),
        )]);

        assert_eq!(goods(&an_npc(), &inventory), vec![(A_VIAL, 0, 2)]);
    }

    #[test]
    fn a_fluid_offer_is_found_only_by_its_subtype() {
        let npc = an_npc();

        assert_eq!(
            npc.offer(A_VIAL, FluidType::Milk as u8)
                .map(|offer| offer.buy),
            Some(Some(8))
        );
        assert!(npc.offer(A_VIAL, FluidType::Water as u8).is_none());
    }
}
