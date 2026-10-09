use std::sync::Arc;

use once_cell::sync::Lazy;
use tracing::error;

use crate::{
    constants::items::MAX_STACK_AMOUNT,
    entities::{
        agent::AgentKey,
        inventory::{Inventory, InventorySlot},
        items::{Item, ItemConfig, ItemGuid, ItemId},
        position::ItemPlacement,
    },
    game::{
        TickCtx,
        config::GAME_CONFIG,
        item_movement::{insert_item_at, stow_item},
    },
    persistence::items::ITEM_CONFIGS,
};

/// Cheapest first.
pub struct Coins(Vec<(Arc<ItemConfig>, u64)>);

pub static COINS: Lazy<Coins> = Lazy::new(|| {
    let money = &GAME_CONFIG.money;
    Coins::new(
        [
            (money.gold_coin_id, 1),
            (money.platinum_coin_id, 100),
            (money.crystal_coin_id, 10_000),
        ]
        .map(|(id, worth)| {
            let config = ITEM_CONFIGS
                .get(&id)
                .unwrap_or_else(|| panic!("coin {id} is not in the item catalogue"));
            (Arc::clone(config), worth)
        }),
    )
});

impl Coins {
    pub fn new(coins: impl IntoIterator<Item = (Arc<ItemConfig>, u64)>) -> Self {
        let mut coins: Vec<_> = coins.into_iter().collect();
        coins.sort_by_key(|(_, worth)| *worth);
        Coins(coins)
    }

    fn worth(&self, item: &Item) -> Option<u64> {
        self.0
            .iter()
            .find(|(config, _)| config.id == item.id())
            .map(|(_, worth)| *worth)
    }

    pub fn make_change(&self, mut amount: u64) -> Vec<Item> {
        let mut change = Vec::new();
        for (config, worth) in self.0.iter().rev() {
            let mut count = amount / worth;
            amount %= worth;
            while count > 0 {
                let stack = count.min(MAX_STACK_AMOUNT as u64);
                change.push(Item::new(Arc::clone(config), stack as u8));
                count -= stack;
            }
        }
        change
    }

    #[cfg(test)]
    pub fn cheapest(&self) -> Arc<ItemConfig> {
        Arc::clone(&self.0[0].0)
    }
}

#[derive(Debug, PartialEq)]
pub struct Payment {
    takes: Vec<(InventorySlot, ItemGuid, u8)>,
    change: u64,
}

impl Payment {
    pub fn and(mut self, other: Payment) -> Payment {
        self.takes.extend(other.takes);
        self.change += other.change;
        self
    }
}

/// One `id` from anywhere in `inventory`, containers included.
pub fn plan_item_payment(inventory: &Inventory, id: ItemId) -> Option<Payment> {
    inventory.slots().iter().find_map(|(slot, item)| {
        find_item(item, id).map(|guid| Payment {
            takes: vec![(*slot, guid, 1)],
            change: 0,
        })
    })
}

fn find_item(item: &Item, id: ItemId) -> Option<ItemGuid> {
    if item.id() == id {
        return Some(item.guid);
    }
    item.content
        .iter()
        .flat_map(|content| content.iter())
        .find_map(|inner| find_item(inner, id))
}

pub fn plan_payment(inventory: &Inventory, coins: &Coins, cost: u64) -> Option<Payment> {
    let mut stacks = Vec::new();
    for (slot, item) in inventory.slots() {
        collect_coins(*slot, item, coins, &mut stacks);
    }
    stacks.sort_by_key(|(_, _, worth, _)| *worth);

    let mut takes = Vec::new();
    let mut remaining = cost;
    for (slot, guid, worth, amount) in stacks {
        if remaining == 0 {
            break;
        }
        let taken = remaining.div_ceil(worth).min(amount as u64);
        takes.push((slot, guid, taken as u8));
        let paid = taken * worth;
        if paid >= remaining {
            return Some(Payment {
                takes,
                change: paid - remaining,
            });
        }
        remaining -= paid;
    }

    (remaining == 0).then_some(Payment { takes, change: 0 })
}

pub fn coins_held(inventory: &Inventory, coins: &Coins) -> u64 {
    let mut stacks = Vec::new();
    for (slot, item) in inventory.slots() {
        collect_coins(*slot, item, coins, &mut stacks);
    }
    stacks
        .iter()
        .map(|(_, _, worth, amount)| worth * *amount as u64)
        .sum()
}

pub fn pay(ctx: &mut TickCtx, player: AgentKey, payment: Payment, coins: &Coins) {
    for (slot, guid, amount) in payment.takes {
        let removed = ctx
            .map
            .player_mut(player)
            .and_then(|mut player| player.inventory_mut().remove(slot, &guid, amount));
        if removed.is_none() {
            error!("payment planned for {player:?} no longer matches their inventory");
            return;
        }
    }

    for coin in coins.make_change(payment.change) {
        give_change(ctx, player, coin);
    }
}

pub fn give_change(ctx: &mut TickCtx, player: AgentKey, coin: Item) {
    if stow_item(ctx, player, coin.clone()).is_ok() {
        return;
    }
    let Some(feet) = ctx.map.agent_position(player).cloned() else {
        error!(
            "{player:?} vanished before receiving {} change",
            coin.amount
        );
        return;
    };
    if let Err(e) = insert_item_at(ctx, coin, &ItemPlacement::Map(feet), None) {
        error!("change for {player:?} could not be dropped at their feet: {e}");
    }
}

fn collect_coins(
    slot: InventorySlot,
    item: &Item,
    coins: &Coins,
    stacks: &mut Vec<(InventorySlot, ItemGuid, u64, u8)>,
) {
    if let Some(worth) = coins.worth(item) {
        stacks.push((slot, item.guid, worth, item.amount));
    }
    for inner in item.content.iter().flat_map(|content| content.iter()) {
        collect_coins(slot, inner, coins, stacks);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{
        agent::Agent,
        map::{GameMap, MapTile},
        position::Position,
        world_map::WorldMap,
    };
    use crate::entities::{
        inventory::Wearer,
        items::{ItemAttribute, ItemFlag, ItemId},
        vocation::Vocation,
    };
    use crate::game::TestHarness;
    use crate::persistence::test_fixtures::a_test_snapshot;
    use std::collections::HashMap;

    fn a_coin_config(id: u16) -> Arc<ItemConfig> {
        Arc::new(ItemConfig::new(
            ItemId(id),
            format!("coin {id}"),
            None,
            None,
            [ItemFlag::Take, ItemFlag::Cumulative],
            [ItemAttribute::Weight(10)],
        ))
    }

    fn coins() -> Coins {
        Coins::new([
            (a_coin_config(3043), 10_000),
            (a_coin_config(3031), 1),
            (a_coin_config(3035), 100),
        ])
    }

    fn a_coin(coins: &Coins, worth: u64, amount: u8) -> Item {
        let (config, _) = coins.0.iter().find(|(_, w)| *w == worth).unwrap();
        Item::new(Arc::clone(config), amount)
    }

    fn a_bag(capacity: u8, content: Vec<Item>) -> Item {
        let mut bag = Item::new(
            Arc::new(ItemConfig::new(
                ItemId(2853),
                "bag".to_string(),
                None,
                None,
                [ItemFlag::Container],
                [ItemAttribute::Capacity(capacity), ItemAttribute::Weight(10)],
            )),
            1,
        );
        bag.content = Some(Box::new(content));
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
    fn an_exact_amount_in_gold_leaves_no_change() {
        let coins = coins();
        let gold = a_coin(&coins, 1, 50);
        let guid = gold.guid;
        let inventory = an_inventory(vec![(InventorySlot::Backpack, a_bag(20, vec![gold]))]);

        assert_eq!(
            plan_payment(&inventory, &coins, 50),
            Some(Payment {
                takes: vec![(InventorySlot::Backpack, guid, 50)],
                change: 0,
            })
        );
    }

    #[test]
    fn the_cheapest_coins_are_spent_first() {
        let coins = coins();
        let gold = a_coin(&coins, 1, 50);
        let guid = gold.guid;
        let platinum = a_coin(&coins, 100, 1);
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(20, vec![platinum, gold]),
        )]);

        assert_eq!(
            plan_payment(&inventory, &coins, 30),
            Some(Payment {
                takes: vec![(InventorySlot::Backpack, guid, 30)],
                change: 0,
            })
        );
    }

    #[test]
    fn coins_in_a_nested_bag_count() {
        let coins = coins();
        let gold = a_coin(&coins, 1, 20);
        let guid = gold.guid;
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(20, vec![a_bag(8, vec![gold])]),
        )]);

        assert_eq!(
            plan_payment(&inventory, &coins, 20),
            Some(Payment {
                takes: vec![(InventorySlot::Backpack, guid, 20)],
                change: 0,
            })
        );
    }

    #[test]
    fn a_coin_worth_more_than_the_cost_is_broken_into_change() {
        let coins = coins();
        let platinum = a_coin(&coins, 100, 1);
        let guid = platinum.guid;
        let inventory = an_inventory(vec![(InventorySlot::Backpack, a_bag(20, vec![platinum]))]);

        assert_eq!(
            plan_payment(&inventory, &coins, 40),
            Some(Payment {
                takes: vec![(InventorySlot::Backpack, guid, 1)],
                change: 60,
            })
        );
    }

    #[test]
    fn too_little_money_plans_nothing() {
        let coins = coins();
        let inventory = an_inventory(vec![(
            InventorySlot::Backpack,
            a_bag(20, vec![a_coin(&coins, 1, 39)]),
        )]);

        assert_eq!(plan_payment(&inventory, &coins, 40), None);
    }

    #[test]
    fn a_cost_of_nothing_is_an_empty_payment() {
        let inventory = an_inventory(vec![]);

        assert_eq!(
            plan_payment(&inventory, &coins(), 0),
            Some(Payment {
                takes: vec![],
                change: 0,
            })
        );
    }

    #[test]
    fn an_item_price_takes_one_from_a_stack_in_a_bag() {
        let coins = coins();
        let gold = a_coin(&coins, 1, 5);
        let guid = gold.guid;
        let inventory = an_inventory(vec![(InventorySlot::Backpack, a_bag(20, vec![gold]))]);

        assert_eq!(
            plan_item_payment(&inventory, ItemId(3031)),
            Some(Payment {
                takes: vec![(InventorySlot::Backpack, guid, 1)],
                change: 0,
            })
        );
        assert_eq!(plan_item_payment(&inventory, ItemId(3035)), None);
    }

    fn a_rope() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(3003),
                "rope".to_string(),
                None,
                None,
                [ItemFlag::Take],
                [ItemAttribute::Weight(1800)],
            )),
            1,
        )
    }

    fn a_player_holding(slots: Vec<(InventorySlot, Item)>) -> (WorldMap, AgentKey, Position) {
        let here = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(here.clone(), MapTile::new());
        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.inventory = HashMap::from_iter(slots);
        let player = map
            .insert_agent(Agent::from_player(snapshot), &here)
            .unwrap();
        (WorldMap::new(map), player, here)
    }

    fn money_held(map: &WorldMap, player: AgentKey, coins: &Coins) -> u64 {
        coins_held(map.get_player(player).unwrap().inventory(), coins)
    }

    fn charge(map: &mut WorldMap, player: AgentKey, coins: &Coins, cost: u64) {
        let payment = plan_payment(map.get_player(player).unwrap().inventory(), coins, cost)
            .expect("the player can afford the cost");
        let mut h = TestHarness::new();
        pay(&mut h.ctx(map), player, payment, coins);
    }

    #[test]
    fn coins_held_adds_up_every_stack_in_every_slot_and_container() {
        let coins = coins();
        let (map, player, _) = a_player_holding(vec![
            (InventorySlot::RightHand, a_coin(&coins, 100, 3)),
            (
                InventorySlot::Backpack,
                a_bag(
                    20,
                    vec![
                        a_coin(&coins, 1, 5),
                        a_bag(20, vec![a_coin(&coins, 10_000, 1)]),
                    ],
                ),
            ),
        ]);

        assert_eq!(
            coins_held(map.get_player(player).unwrap().inventory(), &coins),
            10_305
        );
    }

    #[test]
    fn paying_takes_the_coins_and_returns_the_change() {
        let coins = coins();
        let (mut map, player, _) = a_player_holding(vec![(
            InventorySlot::Backpack,
            a_bag(20, vec![a_coin(&coins, 100, 1)]),
        )]);

        charge(&mut map, player, &coins, 40);

        let backpack = map
            .get_player(player)
            .unwrap()
            .inventory()
            .get(&InventorySlot::Backpack)
            .unwrap();
        let content: Vec<_> = backpack
            .content
            .as_ref()
            .unwrap()
            .iter()
            .map(|item| (item.id(), item.amount))
            .collect();
        assert_eq!(content, vec![(ItemId(3031), 60)]);
    }

    #[test]
    fn change_that_fits_in_no_container_lands_at_the_players_feet() {
        let coins = coins();
        let (mut map, player, here) = a_player_holding(vec![
            (InventorySlot::RightHand, a_coin(&coins, 100, 1)),
            (InventorySlot::Backpack, a_bag(1, vec![a_rope()])),
        ]);

        charge(&mut map, player, &coins, 40);

        assert_eq!(money_held(&map, player, &coins), 0);
        let on_the_floor: Vec<_> = map
            .iter_items(&here)
            .unwrap()
            .map(|item| (item.id(), item.amount))
            .collect();
        assert_eq!(on_the_floor, vec![(ItemId(3031), 60)]);
    }

    #[test]
    fn change_is_made_of_the_fewest_coins() {
        let change: Vec<_> = coins()
            .make_change(9_999)
            .iter()
            .map(|item| (item.id(), item.amount))
            .collect();

        assert_eq!(change, vec![(ItemId(3035), 99), (ItemId(3031), 99)]);
    }

    #[test]
    fn every_configured_coin_is_in_the_item_catalogue() {
        Lazy::force(&COINS);
    }
}
