use anyhow::Result;

use crate::actors::session::SessionActor;
use crate::actors::world::WorldCommand;
use crate::entities::agent::AgentKey;
use crate::entities::items::ItemId;
use crate::entities::map::GameMap;
use crate::game::money::COINS;
use crate::game::shop::{self, Trade};
use crate::messages::{ServerMessage, ShopOfferEntry};

impl SessionActor {
    pub(super) async fn handle_trade(
        &self,
        trade: Trade,
        item_id: ItemId,
        subtype: u8,
        amount: u8,
    ) -> Result<()> {
        self.world
            .send(WorldCommand::Trade {
                agent_key: self.player_key,
                trade,
                item_id,
                subtype,
                amount,
            })
            .await;
        Ok(())
    }

    pub(super) async fn handle_close_shop(&self) -> Result<()> {
        self.world
            .send(WorldCommand::CloseShop {
                agent_key: self.player_key,
            })
            .await;
        Ok(())
    }

    pub(super) async fn send_shop(&self) -> Result<()> {
        let message = shop_message(&self.shared_map.load(), self.player_key);
        if let Some(message) = message {
            self.connection.send_message(message).await?;
        }
        Ok(())
    }

    pub(super) async fn refresh_shop(&self) -> Result<()> {
        let open = self
            .shared_map
            .load()
            .get_player(self.player_key)
            .is_some_and(|player| player.shop().is_some());
        if open {
            self.send_shop().await?;
        }
        Ok(())
    }
}

/// `None` once the player has left the map.
fn shop_message(map: &GameMap, player_key: AgentKey) -> Option<ServerMessage> {
    let player = map.get_player(player_key)?;
    let Some(npc) = player
        .shop()
        .and_then(|npc_key| map.get_agent(npc_key))
        .and_then(|npc| npc.get_npc())
    else {
        return Some(ServerMessage::ShopClosed);
    };
    Some(ServerMessage::Shop {
        npc_name: npc.name.clone(),
        money: shop::money(player, &COINS),
        offers: npc
            .shop
            .iter()
            .map(|offer| ShopOfferEntry {
                item_id: offer.id(),
                subtype: offer.subtype(),
                weight: offer.config.attr_weight().unwrap_or(0),
                buy: offer.buy.unwrap_or(0),
                sell: offer.sell.unwrap_or(0),
            })
            .collect(),
        goods: shop::goods(npc, player.inventory()),
    })
}
