use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use rustibia_server::entities::spells::{SpellGroup, SpellId};
use tokio::sync::mpsc::Sender;

use crate::metrics::{Counter, Event, Probe};

/// A bound on the walk/ping in-flight queues. Sized well above any plausible
/// number of outstanding requests a single bot issues at once, so it only
/// ever bites when the server has stopped answering.
const PENDING_QUEUE_CAP: usize = 16;

/// At most one container open is ever legitimately outstanding: the backpack
/// opens once at login and stays open, and the loot job tracks a single
/// corpse at a time. A retried `UseItem` therefore replaces whatever open it
/// superseded rather than queuing behind it — the earlier fix (a FIFO with
/// this cap raised) still leaked one entry per retried corpse, because only
/// one `OpenContainer` ever answers two `UseItem`s.
const CONTAINER_QUEUE_CAP: usize = 1;

fn push_bounded<T>(queue: &mut VecDeque<T>, value: T, cap: usize) -> bool {
    let dropped = queue.len() >= cap;
    if dropped {
        queue.pop_front();
    }
    queue.push_back(value);
    dropped
}

/// Every in-flight request this bot is waiting on a reply for, and the
/// tallies that share its outbound channel. Centralising them here is what
/// makes "every probe has a cancel path" a property of one type instead of
/// something a reader has to verify at each call site: `open` and `resolve`
/// pair a send with the specific message that answers it, and a reply that
/// carries no useful sample — a denial, a superseded retry — goes through
/// `cancel` so it can never be timed against a later, unrelated reply.
pub struct Probes {
    tx: Sender<Event>,
    walk: VecDeque<Instant>,
    ping: VecDeque<Instant>,
    container: VecDeque<Instant>,
    /// Keyed by `SpellId` rather than a FIFO: the `SpellCast` reply names its
    /// own spell, so a refused cast's stale entry is simply overwritten by
    /// that spell's next attempt instead of shifting every later reply onto
    /// the wrong group's cooldown.
    casts: HashMap<SpellId, SpellGroup>,
    received_messages: u64,
    last_bytes: u64,
}

impl Probes {
    pub fn new(tx: Sender<Event>) -> Self {
        Self {
            tx,
            walk: VecDeque::new(),
            ping: VecDeque::new(),
            container: VecDeque::new(),
            casts: HashMap::new(),
            received_messages: 0,
            last_bytes: 0,
        }
    }

    fn queue_mut(&mut self, probe: Probe) -> &mut VecDeque<Instant> {
        match probe {
            Probe::WalkAck => &mut self.walk,
            Probe::Ping => &mut self.ping,
            Probe::OpenContainer => &mut self.container,
            Probe::DecisionLag => {
                unreachable!(
                    "DecisionLag is sampled directly; it never has a send to pair with a reply"
                )
            }
        }
    }

    fn cap_for(probe: Probe) -> usize {
        match probe {
            Probe::OpenContainer => CONTAINER_QUEUE_CAP,
            _ => PENDING_QUEUE_CAP,
        }
    }

    /// Records that a request was just sent; `resolve` or `cancel` must
    /// eventually pair with it.
    pub async fn open(&mut self, probe: Probe, now: Instant) {
        let cap = Self::cap_for(probe);
        if push_bounded(self.queue_mut(probe), now, cap) {
            let _ = self.tx.send(Event::Dropped(probe)).await;
        }
    }

    /// The server's answer to the oldest outstanding request for `probe`;
    /// samples the round trip. `None` if nothing was pending.
    pub async fn resolve(&mut self, probe: Probe) -> Option<Duration> {
        let sent_at = self.queue_mut(probe).pop_front()?;
        let elapsed = sent_at.elapsed();
        let _ = self.tx.send(Event::Sampled(probe, elapsed)).await;
        Some(elapsed)
    }

    /// The oldest outstanding request for `probe` is known to be abandoned
    /// — a denial, or a retry that supersedes it — and must not be timed
    /// against whatever reply arrives next.
    pub fn cancel(&mut self, probe: Probe) {
        self.queue_mut(probe).pop_front();
    }

    pub fn open_cast(&mut self, spell_id: SpellId, group: SpellGroup) {
        self.casts.insert(spell_id, group);
    }

    pub fn resolve_cast(&mut self, spell_id: SpellId) -> Option<SpellGroup> {
        self.casts.remove(&spell_id)
    }

    pub async fn sample(&self, probe: Probe, duration: Duration) {
        let _ = self.tx.send(Event::Sampled(probe, duration)).await;
    }

    pub async fn count(&self, counter: Counter, n: u64) {
        if n > 0 {
            let _ = self.tx.send(Event::Counted(counter, n)).await;
        }
    }

    pub fn note_incoming(&mut self) {
        self.received_messages += 1;
    }

    /// Flushes the inbound message/byte tally accumulated since the last
    /// flush. Called at the end of every exit path, not only a decision tick.
    pub async fn flush_received(&mut self, bytes_read: u64) {
        let bytes = bytes_read.saturating_sub(self.last_bytes);
        self.last_bytes = bytes_read;
        if self.received_messages > 0 || bytes > 0 {
            let _ = self
                .tx
                .send(Event::Received {
                    messages: self.received_messages,
                    bytes,
                })
                .await;
            self.received_messages = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::drain;

    fn probes() -> (Probes, tokio::sync::mpsc::Receiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        (Probes::new(tx), rx)
    }

    /// The mirror of the single-slot bias the FIFO itself was built to fix:
    /// a denial must cancel its pending entry, or the next genuine reply is
    /// timed against a strictly older send and every later one compounds it.
    #[tokio::test]
    async fn a_cancelled_send_is_never_matched_by_a_later_reply() {
        let (mut probes, _rx) = probes();

        probes.open(Probe::WalkAck, Instant::now()).await; // MovePlayer #1
        tokio::time::sleep(Duration::from_millis(50)).await;
        probes.cancel(Probe::WalkAck); // PlayerWalkDenied

        let second_send = Instant::now();
        probes.open(Probe::WalkAck, second_send).await; // MovePlayer #2
        let sample = probes.resolve(Probe::WalkAck).await.unwrap(); // PlayerWalkAck

        assert!(
            sample < Duration::from_millis(50),
            "the sample ({sample:?}) must be bounded by the second send's elapsed time, \
             not the cancelled first one"
        );
    }

    #[tokio::test]
    async fn a_second_container_open_before_the_first_resolves_replaces_it_and_counts_a_drop() {
        let (mut probes, mut rx) = probes();

        probes.open(Probe::OpenContainer, Instant::now()).await; // the first corpse attempt
        tokio::time::sleep(Duration::from_millis(30)).await;
        let retry_send = Instant::now();
        probes.open(Probe::OpenContainer, retry_send).await; // the retry

        assert!(drain(&mut rx).contains(&Event::Dropped(Probe::OpenContainer)));

        let sample = probes.resolve(Probe::OpenContainer).await.unwrap();
        assert!(
            sample < Duration::from_millis(30),
            "the one OpenContainer reply must resolve the retry, not the superseded first attempt"
        );
    }

    #[test]
    fn casts_resolve_by_their_own_spell_id_regardless_of_send_order() {
        let (mut probes, _rx) = probes();

        probes.open_cast(SpellId(1), SpellGroup::Attack);
        probes.open_cast(SpellId(2), SpellGroup::Healing);

        assert_eq!(probes.resolve_cast(SpellId(2)), Some(SpellGroup::Healing));
        assert_eq!(probes.resolve_cast(SpellId(1)), Some(SpellGroup::Attack));
    }

    #[test]
    fn a_refused_casts_stale_entry_is_overwritten_by_its_next_attempt() {
        let (mut probes, _rx) = probes();

        probes.open_cast(SpellId(1), SpellGroup::Attack);
        // the reply never comes (the cast was refused); the next attempt
        // overwrites the stale entry instead of leaving it to mis-key a
        // future, unrelated cast of the same spell.
        probes.open_cast(SpellId(1), SpellGroup::Attack);

        assert_eq!(probes.resolve_cast(SpellId(1)), Some(SpellGroup::Attack));
        assert_eq!(probes.resolve_cast(SpellId(1)), None);
    }
}
