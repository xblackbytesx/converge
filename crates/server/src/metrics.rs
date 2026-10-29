//! Prometheus metrics, written by hand in the text format (no dependency).
//! Recorded in one place, the proxy's usage guard. No person or key labels:
//! privacy, and it keeps the series bounded by what the admin created.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

/// Seconds; model loads on llama-swap reach the top buckets.
const BUCKETS: [f64; 13] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0];

#[derive(Default, Clone)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    sum: f64,
    count: u64,
}

impl Histogram {
    fn observe(&mut self, d: Duration) {
        let s = d.as_secs_f64();
        for (i, b) in BUCKETS.iter().enumerate() {
            if s <= *b {
                self.counts[i] += 1;
            }
        }
        self.sum += s;
        self.count += 1;
    }
}

/// One finished request, as the usage guard saw it.
pub struct Done<'a> {
    pub endpoint: &'static str,
    pub model: &'a str,
    pub persona: &'a str,
    pub connection: &'a str,
    /// `2xx`, `4xx`, `5xx`, or `limited`.
    pub status: &'static str,
    pub prompt: i64,
    pub completion: i64,
    pub cached: i64,
    pub ttfb: Option<Duration>,
    pub total: Duration,
    /// connect, timeout, stalled, broke off, unreadable.
    pub failure: Option<&'static str>,
    pub limited: Option<&'static str>,
}

#[derive(Default)]
struct Inner {
    requests: BTreeMap<(&'static str, String, String, &'static str), u64>,
    tokens: BTreeMap<(String, &'static str), u64>,
    ttfb: BTreeMap<String, Histogram>,
    total: BTreeMap<String, Histogram>,
    inflight: BTreeMap<String, i64>,
    failures: BTreeMap<(String, &'static str), u64>,
    limited: BTreeMap<&'static str, u64>,
}

#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

/// What `render` needs from outside the registry.
pub struct Gauges {
    pub usage_dropped: u64,
    pub usage_queue: usize,
    pub snapshot_reloads: u64,
}

impl Metrics {
    pub fn started(&self, connection: &str) {
        let mut m = self.inner.lock().expect("metrics lock");
        *m.inflight.entry(connection.to_string()).or_default() += 1;
    }

    pub fn ended(&self, connection: &str) {
        let mut m = self.inner.lock().expect("metrics lock");
        *m.inflight.entry(connection.to_string()).or_default() -= 1;
    }

    pub fn done(&self, d: Done<'_>) {
        let mut m = self.inner.lock().expect("metrics lock");
        *m.requests.entry((d.endpoint, d.model.to_string(), d.persona.to_string(), d.status)).or_default() += 1;
        for (kind, n) in [("prompt", d.prompt), ("completion", d.completion), ("cached", d.cached)] {
            if n > 0 {
                *m.tokens.entry((d.model.to_string(), kind)).or_default() += n as u64;
            }
        }
        if let Some(why) = d.limited {
            *m.limited.entry(why).or_default() += 1;
            return;
        }
        if let Some(t) = d.ttfb {
            m.ttfb.entry(d.connection.to_string()).or_default().observe(t);
        }
        m.total.entry(d.connection.to_string()).or_default().observe(d.total);
        if let Some(why) = d.failure {
            *m.failures.entry((d.connection.to_string(), why)).or_default() += 1;
        }
    }

    /// The Prometheus text format.
    pub fn render(&self, g: &Gauges) -> String {
        let m = self.inner.lock().expect("metrics lock");
        let mut o = String::new();
        let head = |o: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        head(&mut o, "converge_build_info", "gauge", "The running version.");
        let _ = writeln!(o, "converge_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));

        head(&mut o, "converge_requests_total", "counter", "Requests that reached a model or persona.");
        for ((e, model, persona, status), n) in &m.requests {
            let _ = writeln!(
                o,
                "converge_requests_total{{endpoint=\"{}\",model=\"{}\",persona=\"{}\",status=\"{status}\"}} {n}",
                esc(e),
                esc(model),
                esc(persona)
            );
        }
        head(&mut o, "converge_tokens_total", "counter", "Tokens by model and kind (cached is part of prompt).");
        for ((model, kind), n) in &m.tokens {
            let _ = writeln!(o, "converge_tokens_total{{model=\"{}\",kind=\"{kind}\"}} {n}", esc(model));
        }
        for (name, help, map) in [
            ("converge_ttfb_seconds", "Time to the upstream's first byte.", &m.ttfb),
            ("converge_request_seconds", "Whole request, to the last byte.", &m.total),
        ] {
            head(&mut o, name, "histogram", help);
            for (conn, h) in map {
                let c = esc(conn);
                for (b, n) in BUCKETS.iter().zip(h.counts) {
                    let _ = writeln!(o, "{name}_bucket{{connection=\"{c}\",le=\"{b}\"}} {n}");
                }
                let _ = writeln!(o, "{name}_bucket{{connection=\"{c}\",le=\"+Inf\"}} {}", h.count);
                let _ = writeln!(o, "{name}_sum{{connection=\"{c}\"}} {}", h.sum);
                let _ = writeln!(o, "{name}_count{{connection=\"{c}\"}} {}", h.count);
            }
        }
        head(&mut o, "converge_inflight_requests", "gauge", "Requests running now, per connection.");
        for (conn, n) in &m.inflight {
            let _ = writeln!(o, "converge_inflight_requests{{connection=\"{}\"}} {n}", esc(conn));
        }
        head(&mut o, "converge_upstream_failures_total", "counter", "Upstreams that failed or broke off.");
        for ((conn, why), n) in &m.failures {
            let _ =
                writeln!(o, "converge_upstream_failures_total{{connection=\"{}\",reason=\"{why}\"}} {n}", esc(conn));
        }
        head(&mut o, "converge_limited_total", "counter", "Requests turned away by a limit.");
        for (why, n) in &m.limited {
            let _ = writeln!(o, "converge_limited_total{{limit=\"{why}\"}} {n}");
        }
        head(&mut o, "converge_usage_dropped_total", "counter", "Usage records dropped: the writer was behind.");
        let _ = writeln!(o, "converge_usage_dropped_total {}", g.usage_dropped);
        head(&mut o, "converge_usage_queue", "gauge", "Usage records waiting for the writer.");
        let _ = writeln!(o, "converge_usage_queue {}", g.usage_queue);
        head(&mut o, "converge_snapshot_reloads_total", "counter", "Snapshot reloads since start.");
        let _ = writeln!(o, "converge_snapshot_reloads_total {}", g.snapshot_reloads);
        o
    }
}

/// Label values: backslash, quote and newline escaped.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done<'a>(model: &'a str, status: &'static str) -> Done<'a> {
        Done {
            endpoint: "chat/completions",
            model,
            persona: "",
            connection: "local",
            status,
            prompt: 10,
            completion: 5,
            cached: 0,
            ttfb: Some(Duration::from_millis(200)),
            total: Duration::from_secs(3),
            failure: None,
            limited: None,
        }
    }

    #[test]
    fn renders_counters_and_histograms() {
        let m = Metrics::default();
        m.started("local");
        m.done(done("local/q\"x", "2xx"));
        m.done(done("local/q\"x", "2xx"));
        m.done(Done { limited: Some("rpm"), ..done("local/q\"x", "limited") });
        m.ended("local");
        let t = m.render(&Gauges { usage_dropped: 0, usage_queue: 2, snapshot_reloads: 7 });
        assert!(t.contains(
            "converge_requests_total{endpoint=\"chat/completions\",model=\"local/q\\\"x\",persona=\"\",status=\"2xx\"} 2"
        ));
        assert!(t.contains("converge_tokens_total{model=\"local/q\\\"x\",kind=\"prompt\"} 30"), "{t}");
        assert!(t.contains("converge_ttfb_seconds_bucket{connection=\"local\",le=\"0.1\"} 0"));
        assert!(t.contains("converge_ttfb_seconds_bucket{connection=\"local\",le=\"0.25\"} 2"));
        assert!(t.contains("converge_request_seconds_count{connection=\"local\"} 2"), "limited is not timed");
        assert!(t.contains("converge_limited_total{limit=\"rpm\"} 1"));
        assert!(t.contains("converge_inflight_requests{connection=\"local\"} 0"));
        assert!(t.contains("converge_usage_queue 2\n"));
        assert!(t.contains("converge_snapshot_reloads_total 7\n"));
    }
}
