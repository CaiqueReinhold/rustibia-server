use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::layer::SubscriberExt;

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
