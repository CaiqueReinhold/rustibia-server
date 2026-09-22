use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arc_swap::ArcSwap;
use slotmap::Key;
use tokio::sync::watch;
use tracing::{debug, error, info};

use crate::actors::world::{WorldActorHandle, WorldCommand};
use crate::entities::agent::AgentKey;
use crate::entities::map::GameMap;
use crate::game::Tick;
use crate::game::creature_behavior::{
    CreatureAction, CreatureBehaviourContext, CreatureState, active_creatures, decide_action,
};
use crate::game::random::Rolls;

/// Creatures are split across this many buckets, one decided per tick, so a creature is
/// revisited every `BEHAVIOUR_BUCKETS` ticks.
const BEHAVIOUR_BUCKETS: u64 = 5;

pub struct CreatureBehaviorActor {
    tick_rx: watch::Receiver<Tick>,
    world: WorldActorHandle,
    shared_map: Arc<ArcSwap<GameMap>>,
    seed: u64,
    states: Arc<Mutex<HashMap<AgentKey, CreatureState>>>,
}

impl CreatureBehaviorActor {
    pub fn start(
        world: WorldActorHandle,
        shared_map: Arc<ArcSwap<GameMap>>,
        tick_rx: watch::Receiver<Tick>,
        seed: u64,
    ) {
        let actor = Self {
            tick_rx,
            world,
            shared_map,
            seed,
            states: Arc::new(Mutex::new(HashMap::new())),
        };
        tokio::spawn(actor.run());
    }

    async fn run(mut self) {
        info!("CreatureBehaviorActor started");
        while self.tick_rx.changed().await.is_ok() {
            let tick = *self.tick_rx.borrow();
            self.process_tick(tick).await;
        }
    }

    async fn process_tick(&mut self, tick: Tick) {
        let map = self.shared_map.load_full();
        let global_seed = self.seed;
        let states = self.states.clone();
        let bucket = tick.0 % BEHAVIOUR_BUCKETS;

        let decide_start = Instant::now();
        let decided = tokio::task::spawn_blocking(move || {
            let Ok(mut states) = states.lock() else {
                return (0usize, Vec::new());
            };
            let active = active_creatures(&map);
            let mut considered = 0usize;
            let actions = active
                .iter()
                .filter(|agent_key| agent_key.data().as_ffi() % BEHAVIOUR_BUCKETS == bucket)
                .flat_map(|agent_key| {
                    considered += 1;
                    let creature_state = states
                        .entry(*agent_key)
                        .or_insert_with(CreatureState::default);
                    let roll = Rolls::stream(global_seed, tick.0, agent_key.data().as_ffi());
                    decide_action(CreatureBehaviourContext {
                        creature: *agent_key,
                        map: &map,
                        roll,
                        world_tick: tick,
                        state: creature_state,
                    })
                })
                .collect::<Vec<CreatureAction>>();
            if bucket == 0 {
                states.retain(|agent_key, _| map.get_agent(*agent_key).is_some());
            }
            (considered, actions)
        })
        .await;
        match decided {
            Ok((considered, actions)) => {
                let decide_elapsed = decide_start.elapsed();
                let send_start = Instant::now();
                let sent = actions.len();
                for action in actions {
                    self.world
                        .send(WorldCommand::from_creature_action(action))
                        .await;
                }
                debug!(
                    "Tick {} bucket {}/{}: {} creatures decided in {:?}, {} actions sent in {:?}",
                    tick,
                    bucket,
                    BEHAVIOUR_BUCKETS,
                    considered,
                    decide_elapsed,
                    sent,
                    send_start.elapsed()
                );
            }
            Err(e) => {
                error!(
                    "Creature behaviour failed to execute for tick {}: {}",
                    tick, e
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;

    /// A skewed split silently restores the spike the buckets exist to remove.
    #[test]
    fn keys_split_evenly_across_the_buckets() {
        let mut slots: SlotMap<AgentKey, ()> = SlotMap::with_key();
        let keys: Vec<AgentKey> = (0..60_000).map(|_| slots.insert(())).collect();

        let mut counts = [0usize; BEHAVIOUR_BUCKETS as usize];
        for key in &keys {
            counts[(key.data().as_ffi() % BEHAVIOUR_BUCKETS) as usize] += 1;
        }

        let expected = keys.len() / BEHAVIOUR_BUCKETS as usize;
        for (bucket, count) in counts.iter().enumerate() {
            assert!(
                count.abs_diff(expected) <= expected / 20,
                "bucket {bucket} holds {count} of {} keys",
                keys.len()
            );
        }
    }
}
