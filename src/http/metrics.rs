//! `GET /metrics`, behind `serve --metrics`: Prometheus counters of the
//! server, one set per [`AppState`](crate::runtime::AppState).
//!
//! Labels are bounded by construction. A route is the template the router
//! matched, never the URI: the path and query hold the addresses people
//! search, which are personal data, and would make every request its own
//! series. Methods outside the standard set share one label for the same
//! reason.

use std::{
    collections::BTreeMap,
    fmt::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
    time::Instant,
};

use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::runtime::AppState;

/// Prometheus reads OpenMetrics when the content type says so.
const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// The route of a request no route matched: the 404 fallbacks.
const UNMATCHED: &str = "unmatched";

/// Upper bounds of the latency buckets, in seconds: 1 ms times powers of 4.
const BUCKETS: [&str; 10] = [
    "0.001", "0.004", "0.016", "0.064", "0.256", "1.024", "4.096", "16.384", "65.536", "262.144",
];
const BOUNDS: [f64; 10] = [
    0.001, 0.004, 0.016, 0.064, 0.256, 1.024, 4.096, 16.384, 65.536, 262.144,
];

// Written by hand, not with prometheus-client: that crate grew the shipped
// binary past CI's size limit for five metrics.
#[derive(Clone, Default)]
pub(crate) struct Metrics(Arc<Inner>);

#[derive(Default)]
struct Inner {
    /// By route, method and status.
    requests: Mutex<BTreeMap<(String, &'static str, u16), u64>>,
    durations: Mutex<BTreeMap<String, Histogram>>,
    in_flight: AtomicI64,
    succeeded: AtomicU64,
    failed: AtomicU64,
    slots: usize,
}

#[derive(Default)]
struct Histogram {
    /// Not cumulative: each observation counts in its own bucket only.
    buckets: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Metrics").finish_non_exhaustive()
    }
}

impl Metrics {
    /// `slots` is how many batches and jobs may run at once.
    pub(crate) fn new(slots: usize) -> Self {
        Self(Arc::new(Inner {
            slots,
            ..Inner::default()
        }))
    }

    pub(crate) fn batch_items(&self, succeeded: usize, failed: usize) {
        self.0
            .succeeded
            .fetch_add(succeeded as u64, Ordering::Relaxed);
        self.0.failed.fetch_add(failed as u64, Ordering::Relaxed);
    }

    fn observe(&self, route: String, method: &'static str, status: u16, seconds: f64) {
        let mut durations = self.0.durations.lock().expect("metrics lock");
        let histogram = durations.entry(route.clone()).or_default();
        let bucket = BOUNDS.partition_point(|bound| *bound < seconds);
        if let Some(count) = histogram.buckets.get_mut(bucket) {
            *count += 1;
        }
        histogram.count += 1;
        histogram.sum += seconds;
        drop(durations);
        *self
            .0
            .requests
            .lock()
            .expect("metrics lock")
            .entry((route, method, status))
            .or_default() += 1;
    }

    /// Route templates are ours and hold no `"` or `\`, so label values
    /// need no escaping.
    fn render(&self, slots_free: usize) -> String {
        let inner = &self.0;
        let mut text = String::new();
        render_family(&mut text, "build", "info", "The server build");
        let _ = writeln!(
            text,
            "open_geocode_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );

        render_family(
            &mut text,
            "http_requests",
            "counter",
            "Requests answered, by matched route, method and status",
        );
        for ((route, method, status), count) in inner.requests.lock().expect("metrics lock").iter()
        {
            let _ = writeln!(
                text,
                "open_geocode_http_requests_total{{route=\"{route}\",method=\"{method}\",status=\"{status}\"}} {count}"
            );
        }

        let name = "open_geocode_http_request_duration_seconds";
        render_family(
            &mut text,
            "http_request_duration_seconds",
            "histogram",
            "Time to the response headers, by matched route",
        );
        let _ = writeln!(text, "# UNIT {name} seconds");
        for (route, histogram) in inner.durations.lock().expect("metrics lock").iter() {
            let _ = writeln!(text, "{name}_sum{{route=\"{route}\"}} {}", histogram.sum);
            let _ = writeln!(
                text,
                "{name}_count{{route=\"{route}\"}} {}",
                histogram.count
            );
            let mut below = 0;
            for (bound, count) in BUCKETS.iter().zip(histogram.buckets) {
                below += count;
                let _ = writeln!(
                    text,
                    "{name}_bucket{{le=\"{bound}\",route=\"{route}\"}} {below}"
                );
            }
            let _ = writeln!(
                text,
                "{name}_bucket{{le=\"+Inf\",route=\"{route}\"}} {}",
                histogram.count
            );
        }

        let gauges = [
            (
                "http_requests_in_flight",
                "Requests being answered now",
                inner.in_flight.load(Ordering::Relaxed),
            ),
            (
                "batch_slots_in_use",
                "Batches and jobs running now",
                inner.slots.saturating_sub(slots_free) as i64,
            ),
            (
                "batch_slots",
                "Batches and jobs that may run at once",
                inner.slots as i64,
            ),
        ];
        render_family(
            &mut text,
            "batch_items",
            "counter",
            "Batch items and job rows answered, by outcome",
        );
        for (outcome, count) in [("succeeded", &inner.succeeded), ("failed", &inner.failed)] {
            let _ = writeln!(
                text,
                "open_geocode_batch_items_total{{outcome=\"{outcome}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
        for (name, help, value) in gauges {
            render_family(&mut text, name, "gauge", help);
            let _ = writeln!(text, "open_geocode_{name} {value}");
        }
        text.push_str("# EOF\n");
        text
    }
}

fn render_family(text: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(text, "# HELP open_geocode_{name} {help}.");
    let _ = writeln!(text, "# TYPE open_geocode_{name} {kind}");
}

/// Counts a request, its latency and the requests in flight. Latency ends
/// with the response headers: a streamed body, as a job's is, runs on
/// after that.
pub(crate) async fn track(
    State(metrics): State<Metrics>,
    request: Request,
    next: Next,
) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or(UNMATCHED, MatchedPath::as_str)
        .to_owned();
    let method = method_label(request.method());
    let started = Instant::now();
    let _in_flight = InFlight::enter(&metrics.0.in_flight);

    let response = next.run(request).await;

    metrics.observe(
        route,
        method,
        response.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    response
}

/// `QUERY` is named because `/geocode` and `/reverse` take it as a batch.
fn method_label(method: &Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "OPTIONS" => "OPTIONS",
        "PATCH" => "PATCH",
        "QUERY" => "QUERY",
        _ => "other",
    }
}

/// Decrements on drop: a client that hangs up mid-request drops the
/// handler's future, and the gauge must still come back down.
struct InFlight<'a>(&'a AtomicI64);

impl<'a> InFlight<'a> {
    fn enter(gauge: &'a AtomicI64) -> Self {
        gauge.fetch_add(1, Ordering::Relaxed);
        Self(gauge)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) async fn metrics(State(state): State<AppState>) -> Response {
    let free = state.batch_slots().available_permits();
    let mut response = state.metrics().render(free).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_cumulative_and_the_text_ends_with_eof() {
        let metrics = Metrics::new(4);
        metrics.observe("/places".into(), "GET", 200, 0.005);
        metrics.observe("/places".into(), "GET", 200, 0.001);
        metrics.observe("/places".into(), "GET", 200, 999.0);
        let text = metrics.render(3);
        let name = "open_geocode_http_request_duration_seconds";
        for sample in [
            format!(r#"{name}_bucket{{le="0.001",route="/places"}} 1"#),
            format!(r#"{name}_bucket{{le="0.004",route="/places"}} 1"#),
            format!(r#"{name}_bucket{{le="0.016",route="/places"}} 2"#),
            format!(r#"{name}_bucket{{le="262.144",route="/places"}} 2"#),
            format!(r#"{name}_bucket{{le="+Inf",route="/places"}} 3"#),
            format!(r#"{name}_count{{route="/places"}} 3"#),
            "open_geocode_batch_slots_in_use 1".to_string(),
            "open_geocode_batch_slots 4".to_string(),
        ] {
            assert!(text.lines().any(|line| line == sample), "{sample}\n{text}");
        }
        assert!(text.ends_with("\n# EOF\n"), "{text}");
    }
}
