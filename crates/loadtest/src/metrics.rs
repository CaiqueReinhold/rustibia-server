use std::collections::BTreeMap;
use std::time::Duration;

use hdrhistogram::Histogram;
use serde::{Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Probe {
    Ping,
    WalkAck,
    OpenContainer,
    DecisionLag,
}

pub const PROBES: [Probe; 4] = [
    Probe::Ping,
    Probe::WalkAck,
    Probe::OpenContainer,
    Probe::DecisionLag,
];

fn probe_name(probe: Probe) -> &'static str {
    match probe {
        Probe::Ping => "ping",
        Probe::WalkAck => "walk_ack",
        Probe::OpenContainer => "open_container",
        Probe::DecisionLag => "decision_lag",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Counter {
    LoginAttempted,
    LoginSucceeded,
    LoginFailed,
    ConnectFailed,
    LoginSendFailed,
    BurstDisconnected,
    SessionDisconnected,
    WalkDenied,
    ActionRefused,
    DecodeError,
    PotionsDry,
    StripMismatch,
}

const COUNTERS: [Counter; 12] = [
    Counter::LoginAttempted,
    Counter::LoginSucceeded,
    Counter::LoginFailed,
    Counter::ConnectFailed,
    Counter::LoginSendFailed,
    Counter::BurstDisconnected,
    Counter::SessionDisconnected,
    Counter::WalkDenied,
    Counter::ActionRefused,
    Counter::DecodeError,
    Counter::PotionsDry,
    Counter::StripMismatch,
];

fn counter_name(counter: Counter) -> &'static str {
    match counter {
        Counter::LoginAttempted => "login_attempted",
        Counter::LoginSucceeded => "login_succeeded",
        Counter::LoginFailed => "login_failed",
        Counter::ConnectFailed => "connect_failed",
        Counter::LoginSendFailed => "login_send_failed",
        Counter::BurstDisconnected => "burst_disconnected",
        Counter::SessionDisconnected => "session_disconnected",
        Counter::WalkDenied => "walk_denied",
        Counter::ActionRefused => "action_refused",
        Counter::DecodeError => "decode_error",
        Counter::PotionsDry => "potions_dry",
        Counter::StripMismatch => "strip_mismatch",
    }
}

/// What a bot reports through its one outbound channel. `run` stamps each
/// event with elapsed-since-start and folds it into `Metrics`. `Dropped` is
/// per `Probe` rather than a flat counter, so a reader can tell which
/// percentile a run of drops invalidated.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Sampled(Probe, Duration),
    Dropped(Probe),
    Counted(Counter, u64),
    Received { messages: u64, bytes: u64 },
}

fn new_histogram() -> Histogram<u64> {
    Histogram::new(3).expect("sigfig 3 is a valid histogram precision")
}

#[derive(Debug, Clone, Serialize)]
struct ProbeSummary {
    samples: u64,
    dropped: u64,
    p50_ms: Option<f64>,
    p90_ms: Option<f64>,
    p99_ms: Option<f64>,
    max_ms: Option<f64>,
}

fn micros_to_ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

fn summarize(histogram: &Histogram<u64>, dropped: u64) -> ProbeSummary {
    let samples = histogram.len();
    let percentile = |p: f64| (samples > 0).then(|| micros_to_ms(histogram.value_at_percentile(p)));
    ProbeSummary {
        samples,
        dropped,
        p50_ms: percentile(50.0),
        p90_ms: percentile(90.0),
        p99_ms: percentile(99.0),
        max_ms: (samples > 0).then(|| micros_to_ms(histogram.max())),
    }
}

#[derive(Debug, Clone)]
pub struct Bucket {
    pub start_secs: f64,
    histograms: BTreeMap<&'static str, Histogram<u64>>,
    drops: BTreeMap<&'static str, u64>,
    counters: BTreeMap<&'static str, u64>,
    messages: u64,
    bytes: u64,
}

impl Bucket {
    fn new(start_secs: f64) -> Self {
        Self {
            start_secs,
            histograms: PROBES
                .into_iter()
                .map(|probe| (probe_name(probe), new_histogram()))
                .collect(),
            drops: PROBES
                .into_iter()
                .map(|probe| (probe_name(probe), 0))
                .collect(),
            counters: COUNTERS.into_iter().map(|c| (counter_name(c), 0)).collect(),
            messages: 0,
            bytes: 0,
        }
    }

    fn record(&mut self, probe: Probe, sample: Duration) {
        let micros = sample.as_micros().clamp(1, u64::MAX as u128) as u64;
        self.histograms
            .get_mut(probe_name(probe))
            .expect("every Probe has an entry")
            .record(micros)
            .expect("a duration-derived value is within the histogram's auto-resizing range");
    }

    fn record_drop(&mut self, probe: Probe) {
        *self
            .drops
            .get_mut(probe_name(probe))
            .expect("every Probe has a drop entry") += 1;
    }

    fn count(&mut self, counter: Counter, n: u64) {
        *self
            .counters
            .get_mut(counter_name(counter))
            .expect("every Counter has an entry") += n;
    }

    fn receive(&mut self, messages: u64, bytes: u64) {
        self.messages += messages;
        self.bytes += bytes;
    }

    fn merge(&mut self, other: &Bucket) {
        for (name, histogram) in &other.histograms {
            self.histograms
                .get_mut(name)
                .expect("both buckets carry every probe")
                .add(histogram)
                .expect("merged histograms share the same auto-resizing bounds");
        }
        for (name, dropped) in &other.drops {
            *self
                .drops
                .get_mut(name)
                .expect("both buckets carry every probe's drop entry") += dropped;
        }
        for (name, count) in &other.counters {
            *self
                .counters
                .get_mut(name)
                .expect("both buckets carry every counter") += count;
        }
        self.messages += other.messages;
        self.bytes += other.bytes;
    }

    pub fn samples(&self, probe: Probe) -> u64 {
        self.histograms[probe_name(probe)].len()
    }

    pub fn dropped(&self, probe: Probe) -> u64 {
        self.drops[probe_name(probe)]
    }

    /// `None` when the probe has no samples in this bucket — a quiet bucket
    /// and a slow one must not both read as "0.0 ms".
    pub fn percentile(&self, probe: Probe, pct: f64) -> Option<f64> {
        (self.samples(probe) > 0)
            .then(|| micros_to_ms(self.histograms[probe_name(probe)].value_at_percentile(pct)))
    }

    pub fn p99(&self, probe: Probe) -> Option<f64> {
        self.percentile(probe, 99.0)
    }

    pub fn count_of(&self, counter: Counter) -> u64 {
        self.counters[counter_name(counter)]
    }

    pub fn messages(&self) -> u64 {
        self.messages
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Default for Bucket {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl Serialize for Bucket {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct BucketDump {
            start_secs: f64,
            counters: BTreeMap<&'static str, u64>,
            probes: BTreeMap<&'static str, ProbeSummary>,
            messages: u64,
            bytes: u64,
        }

        BucketDump {
            start_secs: self.start_secs,
            counters: self.counters.clone(),
            probes: self
                .histograms
                .iter()
                .map(|(name, histogram)| (*name, summarize(histogram, self.drops[name])))
                .collect(),
            messages: self.messages,
            bytes: self.bytes,
        }
        .serialize(serializer)
    }
}

/// Everything a reader needs to know two reports are comparable, and what an
/// aborted bot's last, un-flushed tallies mean for the final bucket.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Meta {
    pub bucket_secs: f64,
    pub started_at_unix: u64,
    pub bot_count: usize,
    pub ramp_secs: f64,
    pub duration_secs: f64,
    pub server: String,
    pub config: String,
    pub routes: Vec<String>,
    pub bots_aborted: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub meta: Meta,
    pub buckets: Vec<Bucket>,
    pub total: Bucket,
}

/// Owned by `run` and written through a `tokio::sync::mpsc` channel so bots
/// never contend on a lock.
pub struct Metrics {
    bucket_duration: Duration,
    buckets: Vec<Bucket>,
}

impl Metrics {
    pub fn new(bucket_duration: Duration) -> Self {
        Self {
            bucket_duration,
            buckets: Vec::new(),
        }
    }

    fn bucket_mut(&mut self, elapsed: Duration) -> &mut Bucket {
        let index = (elapsed.as_secs_f64() / self.bucket_duration.as_secs_f64()).floor() as usize;
        while self.buckets.len() <= index {
            let start_secs = self.buckets.len() as f64 * self.bucket_duration.as_secs_f64();
            self.buckets.push(Bucket::new(start_secs));
        }
        &mut self.buckets[index]
    }

    pub fn record(&mut self, probe: Probe, elapsed: Duration, sample: Duration) {
        self.bucket_mut(elapsed).record(probe, sample);
    }

    pub fn record_drop(&mut self, probe: Probe, elapsed: Duration) {
        self.bucket_mut(elapsed).record_drop(probe);
    }

    pub fn count(&mut self, counter: Counter, elapsed: Duration, n: u64) {
        self.bucket_mut(elapsed).count(counter, n);
    }

    pub fn received(&mut self, elapsed: Duration, messages: u64, bytes: u64) {
        self.bucket_mut(elapsed).receive(messages, bytes);
    }

    pub fn apply(&mut self, event: Event, elapsed: Duration) {
        match event {
            Event::Sampled(probe, sample) => self.record(probe, elapsed, sample),
            Event::Dropped(probe) => self.record_drop(probe, elapsed),
            Event::Counted(counter, n) => self.count(counter, elapsed, n),
            Event::Received { messages, bytes } => self.received(elapsed, messages, bytes),
        }
    }

    pub fn report(&self, meta: Meta) -> Report {
        let mut total = Bucket::default();
        for bucket in &self.buckets {
            total.merge(bucket);
        }
        Report {
            meta,
            buckets: self.buckets.clone(),
            total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_land_in_the_bucket_of_their_offset() {
        let mut metrics = Metrics::new(Duration::from_secs(10));

        metrics.record(
            Probe::WalkAck,
            Duration::from_secs(0),
            Duration::from_millis(20),
        );
        metrics.record(
            Probe::WalkAck,
            Duration::from_secs(9),
            Duration::from_millis(40),
        );
        metrics.record(
            Probe::WalkAck,
            Duration::from_secs(11),
            Duration::from_millis(60),
        );

        let report = metrics.report(Meta::default());

        assert_eq!(report.buckets.len(), 2);
        assert_eq!(report.buckets[0].samples(Probe::WalkAck), 2);
        assert_eq!(report.buckets[1].samples(Probe::WalkAck), 1);
    }

    #[test]
    fn buckets_know_their_own_start_time() {
        let mut metrics = Metrics::new(Duration::from_secs(10));
        metrics.record(
            Probe::WalkAck,
            Duration::from_secs(25),
            Duration::from_millis(1),
        );

        let report = metrics.report(Meta::default());

        assert_eq!(report.buckets.len(), 3);
        assert_eq!(report.buckets[0].start_secs, 0.0);
        assert_eq!(report.buckets[1].start_secs, 10.0);
        assert_eq!(report.buckets[2].start_secs, 20.0);
    }

    #[test]
    fn the_aggregate_percentile_covers_every_bucket() {
        let mut metrics = Metrics::new(Duration::from_secs(10));
        for ms in 1..=100 {
            metrics.record(
                Probe::WalkAck,
                Duration::from_secs(ms % 30),
                Duration::from_millis(ms),
            );
        }

        let report = metrics.report(Meta::default());

        assert_eq!(report.total.samples(Probe::WalkAck), 100);
        assert!(report.total.p99(Probe::WalkAck).unwrap() >= 99.0);
    }

    #[test]
    fn an_untouched_probes_percentile_serialises_as_null() {
        let metrics = Metrics::new(Duration::from_secs(10));

        let json = serde_json::to_value(metrics.report(Meta::default())).unwrap();

        assert!(json["total"]["probes"]["walk_ack"]["p99_ms"].is_null());
        assert_eq!(json["total"]["probes"]["walk_ack"]["samples"], 0);
    }

    #[test]
    fn counters_add_up_by_the_given_amount_and_serialise() {
        let mut metrics = Metrics::new(Duration::from_secs(10));
        metrics.count(Counter::WalkDenied, Duration::from_secs(0), 1);
        metrics.count(Counter::WalkDenied, Duration::from_secs(0), 2);

        let json = serde_json::to_value(metrics.report(Meta::default())).unwrap();

        assert_eq!(json["total"]["counters"]["walk_denied"], 3);
    }

    #[test]
    fn a_counter_from_a_later_bucket_still_reaches_the_total() {
        let mut metrics = Metrics::new(Duration::from_secs(10));
        metrics.count(Counter::SessionDisconnected, Duration::from_secs(0), 1);
        metrics.count(Counter::SessionDisconnected, Duration::from_secs(25), 1);

        let report = metrics.report(Meta::default());

        assert_eq!(report.total.count_of(Counter::SessionDisconnected), 2);
        assert_eq!(report.buckets.len(), 3);
        assert_eq!(report.buckets[2].count_of(Counter::SessionDisconnected), 1);
    }

    #[test]
    fn a_drop_is_tracked_per_probe_not_as_a_flat_counter() {
        let mut metrics = Metrics::new(Duration::from_secs(10));
        metrics.record_drop(Probe::WalkAck, Duration::ZERO);
        metrics.record_drop(Probe::WalkAck, Duration::ZERO);
        metrics.record_drop(Probe::OpenContainer, Duration::ZERO);

        let report = metrics.report(Meta::default());

        assert_eq!(report.total.dropped(Probe::WalkAck), 2);
        assert_eq!(report.total.dropped(Probe::OpenContainer), 1);
        assert_eq!(report.total.dropped(Probe::Ping), 0);
    }

    #[test]
    fn apply_dispatches_every_event_kind() {
        let mut metrics = Metrics::new(Duration::from_secs(10));

        metrics.apply(Event::Counted(Counter::LoginSucceeded, 1), Duration::ZERO);
        metrics.apply(
            Event::Sampled(Probe::Ping, Duration::from_millis(5)),
            Duration::ZERO,
        );
        metrics.apply(Event::Dropped(Probe::Ping), Duration::ZERO);
        metrics.apply(
            Event::Received {
                messages: 3,
                bytes: 90,
            },
            Duration::ZERO,
        );

        let report = metrics.report(Meta::default());
        assert_eq!(report.total.count_of(Counter::LoginSucceeded), 1);
        assert_eq!(report.total.samples(Probe::Ping), 1);
        assert_eq!(report.total.dropped(Probe::Ping), 1);
        assert_eq!(report.total.messages(), 3);
        assert_eq!(report.total.bytes(), 90);
    }

    #[test]
    fn latest_bucket_is_the_most_recently_written_one() {
        let mut metrics = Metrics::new(Duration::from_secs(10));
        assert!(metrics.report(Meta::default()).buckets.last().is_none());

        metrics.record(
            Probe::WalkAck,
            Duration::from_secs(0),
            Duration::from_millis(500),
        );
        metrics.record(
            Probe::WalkAck,
            Duration::from_secs(20),
            Duration::from_millis(5),
        );

        let report = metrics.report(Meta::default());
        let latest = report.buckets.last().unwrap();
        assert_eq!(latest.start_secs, 20.0);
        assert_eq!(latest.samples(Probe::WalkAck), 1);
        assert!(
            (latest.p99(Probe::WalkAck).unwrap() - 5.0).abs() < 0.1,
            "the histogram's own rounding at 3 significant figures, not the bucket lookup, \
             accounts for any difference from 5.0 exactly"
        );
    }
}
