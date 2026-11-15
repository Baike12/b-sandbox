//! Per-sandbox usage ledger — the accounting side of "one workload = one
//! budget". The backend enforces what it can (rlimits, output caps); this
//! ledger observes what actually happened through the API: execs, wall
//! time, output bytes, file operations.
//!
//! One writer per fact: a counter moves only in the handler that causes it,
//! next to the event the same handler already emits. Counting is on success
//! only — a denied or failed operation consumed nothing.

use serde::Serialize;

/// Wall-clock milliseconds since the unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLedger {
    created_at_ms: u64,
    last_activity_ms: u64,
    execs_started: u64,
    execs_finished: u64,
    execs_timed_out: u64,
    /// Cumulative wall time of execs, including timeouts.
    exec_wall_ms: u64,
    /// Cumulative captured stdout+stderr bytes across execs.
    output_bytes: u64,
    file_ops: u64,
}

impl UsageLedger {
    pub fn new(now_ms: u64) -> Self {
        Self {
            created_at_ms: now_ms,
            last_activity_ms: now_ms,
            ..Default::default()
        }
    }

    fn touch(&mut self, now_ms: u64) {
        self.last_activity_ms = now_ms;
    }

    pub fn record_exec_started(&mut self, now_ms: u64) {
        self.touch(now_ms);
        self.execs_started += 1;
    }

    pub fn record_exec_outcome(
        &mut self,
        timed_out: bool,
        wall_ms: u64,
        output_bytes: usize,
        now_ms: u64,
    ) {
        self.touch(now_ms);
        if timed_out {
            self.execs_timed_out += 1;
        } else {
            self.execs_finished += 1;
        }
        self.exec_wall_ms += wall_ms;
        self.output_bytes += output_bytes as u64;
    }

    pub fn record_file_op(&mut self, now_ms: u64) {
        self.touch(now_ms);
        self.file_ops += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> UsageLedger {
        UsageLedger::new(1000)
    }

    #[test]
    fn counters_move_only_with_their_causes() {
        let mut usage = ledger();
        usage.record_exec_started(1010);
        usage.record_exec_outcome(false, 12, 40, 1020);
        usage.record_file_op(1030);

        assert_eq!(usage.execs_started, 1);
        assert_eq!(usage.execs_finished, 1);
        assert_eq!(usage.execs_timed_out, 0);
        assert_eq!(usage.exec_wall_ms, 12);
        assert_eq!(usage.output_bytes, 40);
        assert_eq!(usage.file_ops, 1);
        assert_eq!(usage.created_at_ms, 1000);
        assert_eq!(usage.last_activity_ms, 1030);
    }

    #[test]
    fn timeouts_count_as_timeouts_and_still_account_wall_time() {
        let mut usage = ledger();
        usage.record_exec_started(1010);
        usage.record_exec_outcome(true, 500, 9, 1510);

        assert_eq!(usage.execs_finished, 0);
        assert_eq!(usage.execs_timed_out, 1);
        assert_eq!(usage.exec_wall_ms, 500);
    }

    #[test]
    fn serialized_view_is_camel_case() {
        let mut usage = ledger();
        usage.record_exec_started(1010);
        usage.record_exec_outcome(false, 3, 7, 1013);
        let view = serde_json::to_value(&usage).unwrap();
        assert_eq!(view["createdAtMs"], 1000);
        assert_eq!(view["lastActivityMs"], 1013);
        assert_eq!(view["execsStarted"], 1);
        assert_eq!(view["execWallMs"], 3);
        assert_eq!(view["outputBytes"], 7);
    }
}
