//! Service-level metrics in the Prometheus text exposition format (0.0.4),
//! hand-rolled — one struct, no dependency. Values move in the same handler
//! writes as the usage ledger. The active-sandbox gauge is derived at scrape
//! time from the registry, never stored: one writer per fact.
//!
//! Label cardinality is closed by construction: HTTP labels key on the route
//! pattern from the route table (never the raw path) plus method and status
//! class, so scrapes cannot grow unbounded.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

/// Cumulative latency buckets in seconds. 1ms floor catches fast probes; the
/// 10s ceiling covers slow execs and template commits.
pub const LATENCY_BUCKETS_S: [f64; 11] = [
    0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// One labeled histogram: cumulative bucket counts plus count and µs-sum
/// (µs, not f64 seconds — atomics only, and re-rendering never drifts).
#[derive(Debug, Default)]
pub struct Histogram {
    buckets: [AtomicU64; LATENCY_BUCKETS_S.len()],
    count: AtomicU64,
    sum_us: AtomicU64,
}

impl Histogram {
    pub fn observe(&self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        let us = elapsed.as_micros() as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        for (cell, bound) in self.buckets.iter().zip(LATENCY_BUCKETS_S.iter()) {
            if secs <= *bound {
                cell.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Prometheus rendering for one label set. `bucket_labels` is the label
    /// fragment up to (but excluding) the `le=` pair — `{` for unlabeled, or
    /// `{route="…",` for labeled series. `full_labels` is the complete label
    /// set for `_sum`/`_count` lines (`{}` when unlabeled).
    fn render(&self, name: &str, bucket_labels: &str, full_labels: &str) -> String {
        let load = |cell: &AtomicU64| cell.load(Ordering::Relaxed);
        let count = load(&self.count);
        let sum_s = load(&self.sum_us) as f64 / 1_000_000.0;
        let mut out = String::new();
        for (cell, bound) in self.buckets.iter().zip(LATENCY_BUCKETS_S.iter()) {
            out.push_str(&format!(
                "{name}_bucket{bucket_labels}le=\"{bound}\"}} {}\n",
                load(cell)
            ));
        }
        out.push_str(&format!(
            "{name}_bucket{bucket_labels}le=\"+Inf\"}} {count}\n"
        ));
        out.push_str(&format!("{name}_sum{full_labels} {sum_s}\n"));
        out.push_str(&format!("{name}_count{full_labels} {count}\n"));
        out
    }
}

/// (method, route, status) → request count. Route is the pattern from the
/// route table ("unmatched" for 404s), so cardinality is bounded.
type HttpKey = (String, String, u16);

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
    reaper_sweeps: AtomicU64,
    orphans_removed: AtomicU64,

    http_in_flight: AtomicI64,
    execs_active: AtomicI64,
    event_streams: AtomicI64,

    http_requests: Mutex<BTreeMap<HttpKey, AtomicU64>>,
    http_durations: Mutex<BTreeMap<String, Histogram>>,
    exec_durations: Histogram,
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

    pub fn inc_reaper_sweeps(&self) {
        self.reaper_sweeps.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_orphans_removed(&self, n: u64) {
        self.orphans_removed.fetch_add(n, Ordering::Relaxed);
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

    /// One HTTP request finished: counter by (method, route, status) plus a
    /// per-route duration histogram observation.
    pub fn record_http(&self, method: &str, route: &'static str, status: u16, elapsed: Duration) {
        let key: HttpKey = (method.to_string(), route.to_string(), status);
        if let Ok(mut map) = self.http_requests.lock() {
            map.entry(key)
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut map) = self.http_durations.lock() {
            map.entry(route.to_string()).or_default().observe(elapsed);
        }
    }

    pub fn http_in_flight_inc(&self) {
        self.http_in_flight.fetch_add(1, Ordering::Relaxed);
    }

    pub fn http_in_flight_dec(&self) {
        self.http_in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn exec_active_inc(&self) {
        self.execs_active.fetch_add(1, Ordering::Relaxed);
    }

    pub fn exec_active_dec(&self) {
        self.execs_active.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn event_streams_inc(&self) {
        self.event_streams.fetch_add(1, Ordering::Relaxed);
    }

    pub fn event_streams_dec(&self) {
        self.event_streams.fetch_sub(1, Ordering::Relaxed);
    }

    /// Wall-clock duration of one backend exec, for the latency histogram.
    pub fn record_exec_duration(&self, elapsed: Duration) {
        self.exec_durations.observe(elapsed);
    }

    /// Render the exposition text. `active` is the live sandbox count,
    /// derived from the registry by the caller; `version` feeds the
    /// build_info gauge so restarts are visible in a scrape diff.
    pub fn render(&self, active: u64, version: &str) -> String {
        fn counter(name: &str, help: &str, value: u64) -> String {
            format!("# HELP {name} {help}.\n# TYPE {name} counter\n{name} {value}\n")
        }
        fn gauge(name: &str, help: &str, value: i64) -> String {
            format!("# HELP {name} {help}.\n# TYPE {name} gauge\n{name} {value}\n")
        }
        let load = |cell: &AtomicU64| cell.load(Ordering::Relaxed);
        let load_i = |cell: &AtomicI64| cell.load(Ordering::Relaxed);
        let mut out = String::new();
        out.push_str(&counter(
            "bsandbox_sandboxes_created_total",
            "Total sandboxes created since startup",
            load(&self.sandboxes_created),
        ));
        out.push_str(&counter(
            "bsandbox_sandboxes_terminated_total",
            "Total sandboxes terminated since startup (client deletes, reaper, and shutdown all land here; subtract reaped_total for client deletes)",
            load(&self.sandboxes_terminated),
        ));
        out.push_str(&counter(
            "bsandbox_sandboxes_reaped_total",
            "Total sandboxes terminated by the idle-TTL reaper",
            load(&self.sandboxes_reaped),
        ));
        out.push_str(&counter(
            "bsandbox_reaper_sweeps_total",
            "Total idle-TTL reaper sweeps executed",
            load(&self.reaper_sweeps),
        ));
        out.push_str(&counter(
            "bsandbox_orphans_removed_total",
            "Orphaned sandbox directories removed at startup",
            load(&self.orphans_removed),
        ));
        out.push_str(&format!(
            "# HELP bsandbox_sandboxes_active Sandboxes currently in the registry.\n\
             # TYPE bsandbox_sandboxes_active gauge\n\
             bsandbox_sandboxes_active {active}\n"
        ));
        out.push_str(&format!(
            "# HELP bsandbox_build_info Service build metadata (version label), always 1.\n\
             # TYPE bsandbox_build_info gauge\n\
             bsandbox_build_info{{version=\"{version}\"}} 1\n"
        ));
        out.push_str(&gauge(
            "bsandbox_http_in_flight",
            "HTTP requests currently being served (headers not yet sent)",
            load_i(&self.http_in_flight),
        ));
        out.push_str(&gauge(
            "bsandbox_execs_active",
            "Backend execs currently running",
            load_i(&self.execs_active),
        ));
        out.push_str(&gauge(
            "bsandbox_event_streams_active",
            "Open SSE event streams",
            load_i(&self.event_streams),
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

        // HTTP counters: deterministic (sorted) order so scrapes diff cleanly.
        if let Ok(map) = self.http_requests.lock() {
            out.push_str(
                "# HELP bsandbox_http_requests_total HTTP requests by method, route pattern, and status.\n\
                 # TYPE bsandbox_http_requests_total counter\n",
            );
            for ((method, route, status), cell) in map.iter() {
                out.push_str(&format!(
                    "bsandbox_http_requests_total{{method=\"{method}\",route=\"{route}\",code=\"{status}\"}} {}\n",
                    load(cell)
                ));
            }
        }
        if let Ok(map) = self.http_durations.lock() {
            out.push_str(
                "# HELP bsandbox_http_request_duration_seconds HTTP request latency by route pattern.\n\
                 # TYPE bsandbox_http_request_duration_seconds histogram\n",
            );
            for (route, hist) in map.iter() {
                out.push_str(&hist.render(
                    "bsandbox_http_request_duration_seconds",
                    &format!("{{route=\"{route}\","),
                    &format!("{{route=\"{route}\"}}"),
                ));
            }
        }
        out.push_str(
            "# HELP bsandbox_exec_duration_seconds Backend exec wall-clock latency.\n\
             # TYPE bsandbox_exec_duration_seconds histogram\n",
        );
        out.push_str(
            &self
                .exec_durations
                .render("bsandbox_exec_duration_seconds", "{", "{}"),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_metrics_render_zeroes_with_help_and_types() {
        let metrics = Metrics::default();
        let text = metrics.render(0, env!("CARGO_PKG_VERSION"));
        assert!(text.contains("# TYPE bsandbox_sandboxes_created_total counter"));
        assert!(text.contains("bsandbox_sandboxes_created_total 0"));
        assert!(text.contains("# TYPE bsandbox_sandboxes_active gauge"));
        assert!(text.contains("bsandbox_sandboxes_active 0"));
        assert!(text.contains("bsandbox_http_in_flight 0"));
        assert!(text.contains("bsandbox_execs_active 0"));
        assert!(text.contains("bsandbox_event_streams_active 0"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn counters_move_and_the_active_gauge_is_derived() {
        let metrics = Metrics::default();
        metrics.inc_sandboxes_created();
        metrics.inc_sandboxes_reaped();
        metrics.inc_reaper_sweeps();
        metrics.add_orphans_removed(3);
        metrics.inc_execs_started();
        metrics.record_exec_outcome(true, true, 40);
        metrics.inc_file_ops();

        let text = metrics.render(7, "test");
        assert!(
            text.contains("bsandbox_sandboxes_created_total 1"),
            "{text}"
        );
        assert!(text.contains("bsandbox_sandboxes_reaped_total 1"), "{text}");
        assert!(text.contains("bsandbox_reaper_sweeps_total 1"), "{text}");
        assert!(text.contains("bsandbox_orphans_removed_total 3"), "{text}");
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

    #[test]
    fn gauges_move_up_and_down() {
        let metrics = Metrics::default();
        metrics.http_in_flight_inc();
        metrics.http_in_flight_inc();
        metrics.http_in_flight_dec();
        metrics.exec_active_inc();
        metrics.event_streams_inc();
        metrics.event_streams_dec();
        let text = metrics.render(0, env!("CARGO_PKG_VERSION"));
        assert!(text.contains("bsandbox_http_in_flight 1"), "{text}");
        assert!(text.contains("bsandbox_execs_active 1"), "{text}");
        assert!(text.contains("bsandbox_event_streams_active 0"), "{text}");
    }

    #[test]
    fn http_records_key_on_route_pattern_and_status() {
        let metrics = Metrics::default();
        metrics.record_http("GET", "/health", 200, Duration::from_millis(1));
        metrics.record_http("GET", "/health", 200, Duration::from_millis(2));
        metrics.record_http("POST", "/sandboxes", 201, Duration::from_millis(3));
        let text = metrics.render(0, env!("CARGO_PKG_VERSION"));
        assert!(
            text.contains(
                "bsandbox_http_requests_total{method=\"GET\",route=\"/health\",code=\"200\"} 2"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "bsandbox_http_requests_total{method=\"POST\",route=\"/sandboxes\",code=\"201\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "bsandbox_http_request_duration_seconds_bucket{route=\"/health\",le=\"0.001\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains("bsandbox_http_request_duration_seconds_count{route=\"/health\"} 2"),
            "{text}"
        );
    }

    #[test]
    fn exec_histogram_is_cumulative_with_sum_and_count() {
        let metrics = Metrics::default();
        metrics.record_exec_duration(Duration::from_millis(4));
        metrics.record_exec_duration(Duration::from_millis(30));
        let text = metrics.render(0, env!("CARGO_PKG_VERSION"));
        assert!(
            text.contains("bsandbox_exec_duration_seconds_bucket{le=\"0.005\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("bsandbox_exec_duration_seconds_bucket{le=\"0.05\"} 2"),
            "{text}"
        );
        assert!(
            text.contains("bsandbox_exec_duration_seconds_bucket{le=\"+Inf\"} 2"),
            "{text}"
        );
        assert!(
            text.contains("bsandbox_exec_duration_seconds_count{} 2"),
            "{text}"
        );
        // 4ms + 30ms = 34ms; µs rounding keeps the sum exact.
        assert!(
            text.contains("bsandbox_exec_duration_seconds_sum{} 0.034"),
            "{text}"
        );
    }
}
