use std::sync::Arc;

use arc_swap::ArcSwap;
use opentelemetry::{KeyValue, global};
use tokio::sync::mpsc;

use super::SERVICE_NAME;
use crate::entities::map::GameMap;

pub fn observe_channel<T: Send + 'static>(name: &'static str, tx: &mpsc::Sender<T>) {
    let weak = tx.downgrade();
    global::meter(SERVICE_NAME)
        .u64_observable_gauge(name)
        .with_callback(move |o| {
            if let Some(tx) = weak.upgrade() {
                o.observe((tx.max_capacity() - tx.capacity()) as u64, &[]);
            }
        })
        .build();
}

pub fn observe_map(shared_map: Arc<ArcSwap<GameMap>>) {
    global::meter(SERVICE_NAME)
        .u64_observable_gauge("rustibia.agents")
        .with_callback(move |o| {
            let map = shared_map.load();
            let (mut players, mut creatures) = (0u64, 0u64);
            for (_, agent) in map.iter_agents() {
                if agent.is_creature() {
                    creatures += 1;
                } else {
                    players += 1;
                }
            }
            o.observe(players, &[KeyValue::new("kind", "player")]);
            o.observe(creatures, &[KeyValue::new("kind", "creature")]);
        })
        .build();
}
