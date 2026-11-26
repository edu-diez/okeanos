//! Span recorder: writes closed `tracing` spans as JSONL.
//!
//! Enabled with `--trace [PATH]` or `OKEANOS_TRACE`. Each line is one span:
//! `{"id","parent","name","start_ms","duration_us","fields":{...}}`, so a trace
//! can be reconstructed as a tree (session > iteration > model_call/tool_call).
//! Zero overhead when no subscriber layer is installed.

use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

pub struct TraceLayer {
    out: Mutex<BufWriter<File>>,
}

impl TraceLayer {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            out: Mutex::new(BufWriter::new(File::create(path)?)),
        })
    }

    fn write(&self, value: Value) {
        if let Ok(mut out) = self.out.lock() {
            let _ = writeln!(out, "{value}");
            let _ = out.flush();
        }
    }
}

struct SpanData {
    started: Instant,
    start_ms: u64,
    fields: Map<String, Value>,
}

#[derive(Default)]
struct FieldCollector(Map<String, Value>);

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), json!(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().into(), json!(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), json!(format!("{value:?}")));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl<S> Layer<S> for TraceLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut collector = FieldCollector::default();
        attrs.record(&mut collector);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanData {
                started: Instant::now(),
                start_ms: now_ms(),
                fields: collector.0,
            });
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(data) = span.extensions_mut().get_mut::<SpanData>()
        {
            let mut collector = FieldCollector::default();
            values.record(&mut collector);
            data.fields.extend(collector.0);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut collector = FieldCollector::default();
        event.record(&mut collector);
        self.write(json!({
            "event": event.metadata().level().as_str(),
            "span": ctx.current_span().id().map(|i| i.into_u64()),
            "ts_ms": now_ms(),
            "fields": collector.0,
        }));
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(data) = span.extensions_mut().remove::<SpanData>() else {
            return;
        };
        self.write(json!({
            "id": id.into_u64(),
            "parent": span.parent().map(|p| p.id().into_u64()),
            "name": span.name(),
            "start_ms": data.start_ms,
            "duration_us": data.started.elapsed().as_micros() as u64,
            "fields": data.fields,
        }));
    }
}
