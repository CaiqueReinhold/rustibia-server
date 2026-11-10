use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

pub struct CapturedSpans {
    exporter: InMemorySpanExporter,
    _provider: SdkTracerProvider,
    _guard: DefaultGuard,
}

impl CapturedSpans {
    pub fn finished(&self) -> Vec<SpanData> {
        self.exporter.get_finished_spans().unwrap()
    }
}

/// Routes every `tracing` span on the current thread to an in-memory exporter until the returned
/// value is dropped. A span is exported when its last handle is dropped.
pub fn capture_spans() -> CapturedSpans {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    CapturedSpans {
        exporter,
        _provider: provider,
        _guard: tracing::subscriber::set_default(subscriber),
    }
}

#[derive(Clone, Debug)]
pub struct CapturedEvent {
    pub level: Level,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}

impl Visit for CapturedEvent {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.insert(field.name().to_owned(), text);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
}

pub struct CapturedEvents {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    _guard: DefaultGuard,
}

impl CapturedEvents {
    pub fn all(&self) -> Vec<CapturedEvent> {
        self.events.lock().unwrap().clone()
    }
}

struct EventRecorder(Arc<Mutex<Vec<CapturedEvent>>>);

impl<S: Subscriber> Layer<S> for EventRecorder {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut captured = CapturedEvent {
            level: *event.metadata().level(),
            message: String::new(),
            fields: BTreeMap::new(),
        };
        event.record(&mut captured);
        self.0.lock().unwrap().push(captured);
    }
}

/// Collects every `tracing` event emitted on the current thread until the returned value is
/// dropped.
pub fn capture_events() -> CapturedEvents {
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(EventRecorder(events.clone()));
    CapturedEvents {
        events,
        _guard: tracing::subscriber::set_default(subscriber),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_level_message_and_fields() {
        let events = capture_events();

        tracing::warn!(reason = "evicted", character = %7, "Session ended");

        let all = events.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].level, Level::WARN);
        assert_eq!(all[0].message, "Session ended");
        assert_eq!(all[0].fields["reason"], "evicted");
        assert_eq!(all[0].fields["character"], "7");
    }
}
