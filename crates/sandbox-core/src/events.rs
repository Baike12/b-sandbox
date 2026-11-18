//! Per-sandbox event log: monotonic sequence numbers, bounded capacity.
//!
//! This is the shape b-superagent consumes: a JSONL stream with a single
//! monotonic `seq` per sandbox, read incrementally with an `after` cursor.
//! Capacity is bounded because workloads can generate unbounded events
//! (exec output events, etc.); on overflow the oldest events are dropped and
//! the drop count is reported to readers, never hidden.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Bounded capacity per sandbox log. Chosen to keep memory per sandbox flat.
pub const EVENT_LOG_CAPACITY: usize = 10_000;

/// One sandbox event. `seq` is unique and monotonic per sandbox; `ts_ms` is
/// host wall-clock unix milliseconds (informational, not an ordering key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: u64,
    pub ts_ms: u64,
    pub kind: EventKind,
    pub data: serde_json::Value,
}

/// Closed event vocabulary. Adding a variant is a wire-compatible extension;
/// renaming one is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SandboxCreated,
    SandboxPaused,
    SandboxResumed,
    TemplateCommitted,
    ExecStarted,
    ExecFinished,
    ExecTimedOut,
    SandboxTerminated,
}

/// Bounded, thread-safe per-sandbox event log.
#[derive(Debug)]
pub struct EventLog {
    events: VecDeque<Event>,
    capacity: usize,
    next_seq: u64,
    dropped: u64,
}

impl EventLog {
    pub fn new() -> Self {
        Self::with_capacity(EVENT_LOG_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        // Capacity is tracked explicitly: VecDeque may over-allocate internally,
        // and the drop-at-bound behavior must match the declared capacity.
        let capacity = capacity.max(1);
        Self {
            events: VecDeque::with_capacity(capacity),
            capacity,
            next_seq: 1,
            dropped: 0,
        }
    }

    /// Append an event. `seq` is assigned here; caller-provided seqs do not
    /// exist (single-writer invariant: the log owns its sequence).
    pub fn push(&mut self, kind: EventKind, data: serde_json::Value) -> Event {
        let event = Event {
            seq: self.next_seq,
            ts_ms: now_ms(),
            kind,
            data,
        };
        self.next_seq += 1;
        if self.events.len() >= self.capacity {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(event.clone());
        event
    }

    /// Events with `seq > after`, oldest first. `after = 0` reads from the start.
    pub fn events_after(&self, after: u64) -> Vec<Event> {
        self.events
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect()
    }

    /// Highest assigned sequence number (0 for an empty log).
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// How many events were dropped due to capacity (cumulative).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> EventLog {
        EventLog::with_capacity(4)
    }

    #[test]
    fn seq_is_monotonic_and_starts_at_one() {
        let mut log = log();
        let a = log.push(EventKind::SandboxCreated, serde_json::json!({}));
        let b = log.push(EventKind::ExecStarted, serde_json::json!({}));
        assert_eq!(a.seq, 1);
        assert_eq!(b.seq, 2);
        assert_eq!(log.last_seq(), 2);
    }

    #[test]
    fn after_cursor_reads_forward() {
        let mut log = log();
        for kind in [
            EventKind::SandboxCreated,
            EventKind::ExecStarted,
            EventKind::ExecFinished,
        ] {
            log.push(kind, serde_json::json!({}));
        }
        let all = log.events_after(0);
        assert_eq!(all.len(), 3);
        let tail = log.events_after(2);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 3);
        assert_eq!(log.events_after(3).len(), 0);
    }

    #[test]
    fn overflow_drops_oldest_and_counts() {
        let mut log = log(); // capacity 4
        for i in 0..6u64 {
            log.push(EventKind::ExecStarted, serde_json::json!({ "i": i }));
        }
        // Oldest two (seq 1, 2) dropped.
        assert_eq!(log.dropped(), 2);
        let all = log.events_after(0);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].seq, 3);
        assert_eq!(all[3].seq, 6);
        // Sequence numbers never reuse or regress despite drops.
        assert_eq!(log.last_seq(), 6);
    }

    #[test]
    fn events_serialize_snake_case() {
        let mut log = log();
        let event = log.push(EventKind::TemplateCommitted, serde_json::json!({}));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"kind\":\"template_committed\""), "{json}");
        assert!(json.contains("\"tsMs\""), "{json}");
    }

    #[test]
    fn pause_events_serialize_snake_case() {
        let mut log = log();
        for (kind, name) in [
            (EventKind::SandboxPaused, "sandbox_paused"),
            (EventKind::SandboxResumed, "sandbox_resumed"),
        ] {
            let event = log.push(kind, serde_json::json!({}));
            let json = serde_json::to_string(&event).unwrap();
            assert!(json.contains(&format!("\"kind\":\"{name}\"")), "{json}");
        }
    }
}
