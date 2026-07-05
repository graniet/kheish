//! In-memory ring buffer of the daemon's own tracing events.
//!
//! The buffer feeds `GET /v1/logs`: operators get a live, filterable view
//! of what the whole daemon is doing (scheduler, deliveries, MCP, errors)
//! without shell access to the process log file. It is bounded, so it can
//! never grow the daemon's footprint, and it only retains what the active
//! tracing filter already lets through.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;

/// The number of events retained; old entries fall off the front.
const LOG_BUFFER_CAPACITY: usize = 5_000;

/// One captured tracing event, API-shaped.
#[derive(Clone, Debug, Serialize)]
pub struct DaemonLogEntry {
    /// Capture timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// The tracing level, lowercase (`error`…`trace`).
    pub level: String,
    /// The emitting module target.
    pub target: String,
    /// The event's `message` field, when present.
    pub message: String,
    /// The remaining event fields, stringified.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

static BUFFER: Mutex<VecDeque<DaemonLogEntry>> = Mutex::new(VecDeque::new());

/// Returns up to `limit` most recent entries, oldest first.
pub fn recent_daemon_logs(limit: usize) -> Vec<DaemonLogEntry> {
    let buffer = BUFFER.lock().expect("daemon log buffer poisoned");
    let skip = buffer.len().saturating_sub(limit.min(LOG_BUFFER_CAPACITY));
    buffer.iter().skip(skip).cloned().collect()
}

fn push(entry: DaemonLogEntry) {
    let mut buffer = BUFFER.lock().expect("daemon log buffer poisoned");
    if buffer.len() == LOG_BUFFER_CAPACITY {
        buffer.pop_front();
    }
    buffer.push_back(entry);
}

/// A `tracing` layer mirroring every filtered event into the ring buffer.
pub struct DaemonLogBufferLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DaemonLogBufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);
        push(DaemonLogEntry {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as u64)
                .unwrap_or_default(),
            level: event.metadata().level().as_str().to_ascii_lowercase(),
            target: event.metadata().target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        });
    }
}

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_keeps_the_newest_entries_in_order() {
        for index in 0..(LOG_BUFFER_CAPACITY + 10) {
            push(DaemonLogEntry {
                timestamp_ms: index as u64,
                level: "info".to_string(),
                target: "test".to_string(),
                message: format!("event {index}"),
                fields: BTreeMap::new(),
            });
        }
        let recent = recent_daemon_logs(5);
        assert_eq!(recent.len(), 5);
        assert_eq!(
            recent[4].message,
            format!("event {}", LOG_BUFFER_CAPACITY + 9)
        );
        assert!(recent[0].timestamp_ms < recent[4].timestamp_ms);
        assert_eq!(recent_daemon_logs(usize::MAX).len(), LOG_BUFFER_CAPACITY);
    }
}
