//! Service-level metrics in the Prometheus text exposition format (0.0.4),
//! hand-rolled — one struct, no dependency, values move in the same handler
//! writes as the usage ledger. The active-sandbox gauge is derived at scrape
//! time from the registry, never stored: one writer per fact.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    sandboxes_created: AtomicU64,
    sandboxes_terminated: AtomicU64,
    sandboxes_reaped: AtomicU64,
    execs_started: AtomicU64,
    execs_finished: AtomicU64,
    execs_timed_out: AtomicU64,
    execs_truncated: AtomicU64,
    exec_output_bytes: AtomicU64,
    file_ops: AtomicU64,
}

impl Metrics {
    pub fn inc_sandboxes_created(&self) {
        self.sandboxes_created.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_sandboxes_terminated(&self) {
        self.sandboxes_terminated.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_sandboxes_reaped(&self) {
        self.sandboxes_reaped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_execs_started(&self) {
        self.execs_started.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one exec outcome: finished vs timed out, truncated flag, and
    /// captured output size.
    pub fn record_exec_outcome(&self, timed_out: bool, truncated: bool, output_bytes: usize) {
        if timed_out {
            self.execs_timed_out.fetch_add(1, Ordering::Relaxed);
        } else {
            self.execs_finished.fetch_add(1, Ordering::Relaxed);
        }
        if truncated {
            self.execs_truncated.fetch_add(1, Ordering::Relaxed);
        }
        self.exec_output_bytes
            .fetch_add(output_bytes as u64, Ordering::Relaxed);
    }

    pub fn inc_file_ops(&self) {
        self.file_ops.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the exposition text. `active` is the live sandbox count,
    /// derived from the registry by the caller.
    pub fn render(&self, active: u64) -> String {
        fn counter(name: &str, help: &str, value: u64) -> String {
            format!("# HELP {name} {help}.\n# TYPE {name} counter\n{name} {value}\n")
        }
        let load = |cell: &AtomicU64| cell.load(Ordering::Relaxed);
        let mut out = String::new();
        out.push_str(&counter(
            "bsandbox_sandboxes_created_total",
            "Total sandboxes created since startup",
            load(&self.sandboxes_created),
        ));
        out.push_str(&counter(
            "bsandbox_sandboxes_terminated_total",
            "Total sandboxes terminated since startup (client deletes)",
            load(&self.sandboxes_terminated),
        ));
        out.push_str(&counter(
            "bsandbox_sandboxes_reaped_total",
            "Total sandboxes terminated by the idle-TTL reaper",
            load(&self.sandboxes_reaped),
        ));
        out.push_str(&format!(
            "# HELP bsandbox_sandboxes_active Sandboxes currently in the registry.\n\
             # TYPE bsandbox_sandboxes_active gauge\n\
             bsandbox_sandboxes_active {active}\n"
        ));
        out.push_str(&counter(
            "bsandbox_execs_started_total",
            "Total execs accepted",
            load(&self.execs_started),
        ));
        out.push_str(&counter(
            "bsandbox_execs_finished_total",
            "Total execs that ran to completion",
            load(&self.execs_finished),
        ));
        out.push_str(&counter(
            "bsandbox_execs_timed_out_total",
            "Total execs killed by their timeout",
            load(&self.execs_timed_out),
        ));
        out.push_str(&counter(
            "bsandbox_execs_truncated_total",
            "Total execs whose captured output hit the byte cap",
            load(&self.execs_truncated),
        ));
        out.push_str(&counter(
            "bsandbox_exec_output_bytes_total",
            "Total captured stdout+stderr bytes across execs",
            load(&self.exec_output_bytes),
        ));
        out.push_str(&counter(
            "bsandbox_file_ops_total",
            "Total successful file data-plane operations",
            load(&self.file_ops),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_metrics_render_zeroes_with_help_and_types() {
        let metrics = Metrics::default();
        let text = metrics.render(0);
        assert!(text.contains("# TYPE bsandbox_sandboxes_created_total counter"));
        assert!(text.contains("bsandbox_sandboxes_created_total 0"));
        assert!(text.contains("# TYPE bsandbox_sandboxes_active gauge"));
        assert!(text.contains("bsandbox_sandboxes_active 0"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn counters_move_and_the_active_gauge_is_derived() {
        let metrics = Metrics::default();
        metrics.inc_sandboxes_created();
        metrics.inc_sandboxes_reaped();
        metrics.inc_execs_started();
        metrics.record_exec_outcome(true, true, 40);
        metrics.inc_file_ops();

        let text = metrics.render(7);
        assert!(
            text.contains("bsandbox_sandboxes_created_total 1"),
            "{text}"
        );
        assert!(text.contains("bsandbox_sandboxes_reaped_total 1"), "{text}");
        assert!(text.contains("bsandbox_execs_started_total 1"), "{text}");
        assert!(text.contains("bsandbox_execs_timed_out_total 1"), "{text}");
        assert!(text.contains("bsandbox_execs_truncated_total 1"), "{text}");
        assert!(text.contains("bsandbox_execs_finished_total 0"), "{text}");
        assert!(
            text.contains("bsandbox_exec_output_bytes_total 40"),
            "{text}"
        );
        assert!(text.contains("bsandbox_file_ops_total 1"), "{text}");
        assert!(text.contains("bsandbox_sandboxes_active 7"), "{text}");
    }
}
