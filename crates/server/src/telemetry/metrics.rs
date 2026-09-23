use std::time::Duration;

use once_cell::sync::Lazy;
use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram, Meter},
};

use super::{CommandRecord, SERVICE_NAME};

const TICK_SECONDS: [f64; 16] = [
    0.001, 0.0025, 0.005, 0.01, 0.0125, 0.015, 0.0175, 0.02, 0.03, 0.04, 0.05, 0.075, 0.1, 0.25,
    0.5, 1.0,
];
const COMMAND_SECONDS: [f64; 10] = [1e-6, 5e-6, 1e-5, 5e-5, 1e-4, 5e-4, 1e-3, 5e-3, 1e-2, 5e-2];
const LOGIN_SECONDS: [f64; 10] = [0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
const COUNTS: [f64; 10] = [
    0.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0, 50000.0, 100000.0,
];
const DEPTHS: [f64; 11] = [
    0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0,
];

static METRICS: Lazy<Metrics> = Lazy::new(|| Metrics::new(&global::meter(SERVICE_NAME)));

/// Instruments bind to the meter provider installed when this is first called, so
/// [`super::init`] must run before it.
pub fn metrics() -> &'static Metrics {
    &METRICS
}

pub struct TickTimings {
    pub total: Duration,
    pub drain: Duration,
    pub systems: Duration,
    pub snapshot: Duration,
    pub publish: Duration,
    pub budget: Duration,
    pub drained: u64,
    pub queue_future: u64,
    pub chunk_copies: u64,
}

pub struct Metrics {
    tick_duration: Histogram<f64>,
    tick_overrun: Counter<u64>,
    tick_overrun_duration: Histogram<f64>,
    tick_drained: Histogram<u64>,
    tick_chunk_copies: Histogram<u64>,
    queue_future: Histogram<u64>,
    command_duration: Histogram<f64>,
    behaviour_decide: Histogram<f64>,
    behaviour_creatures: Histogram<u64>,
    behaviour_actions: Histogram<u64>,
    creatures_active: Gauge<u64>,
    sessions_active: Gauge<u64>,
    session_queue_depth: Histogram<u64>,
    session_evicted: Counter<u64>,
    net_frames: Counter<u64>,
    net_bytes: Counter<u64>,
    login: Counter<u64>,
    login_duration: Histogram<f64>,
    records_dropped: Counter<u64>,
}

impl Metrics {
    pub fn new(meter: &Meter) -> Self {
        let seconds = |name: &'static str, bounds: &[f64]| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_boundaries(bounds.to_vec())
                .build()
        };
        let histogram = |name: &'static str, bounds: &[f64]| {
            meter
                .u64_histogram(name)
                .with_boundaries(bounds.to_vec())
                .build()
        };
        Self {
            tick_duration: seconds("rustibia.tick.duration", &TICK_SECONDS),
            tick_overrun: meter.u64_counter("rustibia.tick.overrun").build(),
            tick_overrun_duration: seconds("rustibia.tick.overrun.duration", &TICK_SECONDS),
            tick_drained: histogram("rustibia.tick.commands.drained", &COUNTS),
            tick_chunk_copies: histogram("rustibia.tick.chunk_copies", &COUNTS),
            queue_future: histogram("rustibia.world.queue.future", &COUNTS),
            command_duration: seconds("rustibia.world.command.duration", &COMMAND_SECONDS),
            behaviour_decide: seconds("rustibia.behaviour.decide.duration", &TICK_SECONDS),
            behaviour_creatures: histogram("rustibia.behaviour.creatures", &COUNTS),
            behaviour_actions: histogram("rustibia.behaviour.actions", &COUNTS),
            creatures_active: meter.u64_gauge("rustibia.creatures.active").build(),
            sessions_active: meter.u64_gauge("rustibia.sessions.active").build(),
            session_queue_depth: histogram("rustibia.session.queue.depth", &DEPTHS),
            session_evicted: meter.u64_counter("rustibia.session.evicted").build(),
            net_frames: meter.u64_counter("rustibia.net.frames").build(),
            net_bytes: meter
                .u64_counter("rustibia.net.bytes")
                .with_unit("By")
                .build(),
            login: meter.u64_counter("rustibia.login").build(),
            login_duration: seconds("rustibia.login.duration", &LOGIN_SECONDS),
            records_dropped: meter
                .u64_counter("rustibia.telemetry.records.dropped")
                .build(),
        }
    }

    pub fn record_tick(&self, t: &TickTimings) {
        for (phase, duration) in [
            ("total", t.total),
            ("drain", t.drain),
            ("systems", t.systems),
            ("snapshot", t.snapshot),
            ("publish", t.publish),
        ] {
            self.tick_duration
                .record(duration.as_secs_f64(), &[KeyValue::new("phase", phase)]);
        }
        if t.total > t.budget {
            self.tick_overrun.add(1, &[]);
            self.tick_overrun_duration
                .record((t.total - t.budget).as_secs_f64(), &[]);
        }
        self.tick_drained.record(t.drained, &[]);
        self.tick_chunk_copies.record(t.chunk_copies, &[]);
        self.queue_future.record(t.queue_future, &[]);
    }

    pub fn record_commands(&self, records: &[CommandRecord]) {
        for record in records {
            self.command_duration.record(
                (record.end - record.start).as_secs_f64(),
                &[KeyValue::new("command", record.command)],
            );
        }
    }

    pub fn record_behaviour(&self, decide: Duration, considered: u64, active: u64, actions: u64) {
        self.behaviour_decide.record(decide.as_secs_f64(), &[]);
        self.behaviour_creatures.record(considered, &[]);
        self.behaviour_actions.record(actions, &[]);
        self.creatures_active.record(active, &[]);
    }

    pub fn record_sessions_active(&self, sessions: u64) {
        self.sessions_active.record(sessions, &[]);
    }

    pub fn record_session_queue_depth(&self, depth: u64) {
        self.session_queue_depth.record(depth, &[]);
    }

    pub fn record_session_evicted(&self, reason: &'static str) {
        self.session_evicted
            .add(1, &[KeyValue::new("reason", reason)]);
    }

    pub fn record_frame(&self, direction: &'static str, bytes: u64) {
        let attributes = [KeyValue::new("direction", direction)];
        self.net_frames.add(1, &attributes);
        self.net_bytes.add(bytes, &attributes);
    }

    pub fn record_login(&self, outcome: &'static str, duration: Duration) {
        let attributes = [KeyValue::new("outcome", outcome)];
        self.login.add(1, &attributes);
        self.login_duration
            .record(duration.as_secs_f64(), &attributes);
    }

    pub(super) fn record_dropped_batch(&self) {
        self.records_dropped.add(1, &[]);
    }
}
