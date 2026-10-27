//! `GET /metrics`, behind `serve --metrics`: Prometheus counters of the
//! server, one registry per [`AppState`](crate::runtime::AppState).
//!
//! Labels are bounded by construction. A route is the template the router
//! matched, never the URI: the path and query hold the addresses people
//! search, which are personal data, and would make every request its own
//! series. Methods outside the standard set share one label for the same
//! reason.

use std::{sync::Arc, time::Instant};

use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use prometheus_client::{
    encoding::{EncodeLabelSet, text::encode},
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
        histogram::{Histogram, exponential_buckets},
        info::Info,
    },
    registry::{Registry, Unit},
};

use crate::runtime::AppState;

/// OpenMetrics, which is what `prometheus-client` writes; Prometheus reads
/// it when the content type says so.
const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// The route of a request no route matched: the 404 fallbacks.
const UNMATCHED: &str = "unmatched";

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RequestLabels {
    route: String,
    method: &'static str,
    status: u16,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RouteLabels {
    route: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

#[derive(Clone)]
pub(crate) struct Metrics {
    registry: Arc<Registry>,
    requests: Family<RequestLabels, Counter>,
    duration: Family<RouteLabels, Histogram>,
    in_flight: Gauge,
    batch_items: Family<OutcomeLabels, Counter>,
    slots_in_use: Gauge,
    slots: usize,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Metrics").finish_non_exhaustive()
    }
}

impl Metrics {
    /// `slots` is how many batches may run at once.
    pub(crate) fn new(slots: usize) -> Self {
        let mut registry = Registry::with_prefix("open_geocode");
        let requests = Family::<RequestLabels, Counter>::default();
        let duration = Family::<RouteLabels, Histogram>::new_with_constructor(|| {
            Histogram::new(exponential_buckets(0.001, 4.0, 10))
        });
        let in_flight = Gauge::default();
        let batch_items = Family::<OutcomeLabels, Counter>::default();
        let slots_in_use = Gauge::default();
        let slots_total = Gauge::<i64>::default();
        slots_total.set(slots as i64);

        registry.register(
            "build",
            "The server build",
            Info::new(vec![("version", env!("CARGO_PKG_VERSION"))]),
        );
        registry.register(
            "http_requests",
            "Requests answered, by matched route, method and status",
            requests.clone(),
        );
        registry.register_with_unit(
            "http_request_duration",
            "Time to the response headers, by matched route",
            Unit::Seconds,
            duration.clone(),
        );
        registry.register(
            "http_requests_in_flight",
            "Requests being answered now",
            in_flight.clone(),
        );
        registry.register(
            "batch_items",
            "Batch items answered, by outcome",
            batch_items.clone(),
        );
        registry.register(
            "batch_slots_in_use",
            "Batches running now",
            slots_in_use.clone(),
        );
        registry.register("batch_slots", "Batches that may run at once", slots_total);
        // Both outcomes from the start, so a rate over no items reads 0
        // rather than no data.
        for outcome in ["succeeded", "failed"] {
            let _ = batch_items.get_or_create(&OutcomeLabels { outcome });
        }
        Self {
            registry: Arc::new(registry),
            requests,
            duration,
            in_flight,
            batch_items,
            slots_in_use,
            slots,
        }
    }

    pub(crate) fn batch_items(&self, succeeded: usize, failed: usize) {
        for (outcome, count) in [("succeeded", succeeded), ("failed", failed)] {
            if count > 0 {
                self.batch_items
                    .get_or_create(&OutcomeLabels { outcome })
                    .inc_by(count as u64);
            }
        }
    }

    fn render(&self, slots_free: usize) -> String {
        self.slots_in_use
            .set(self.slots.saturating_sub(slots_free) as i64);
        let mut text = String::new();
        encode(&mut text, &self.registry).expect("encoding into a String cannot fail");
        text
    }
}

/// Counts a request, its latency and the requests in flight. Latency ends
/// with the response headers: a streamed body, as a batch's is, runs on
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
    let _in_flight = InFlight::enter(&metrics.in_flight);

    let response = next.run(request).await;

    metrics
        .duration
        .get_or_create(&RouteLabels {
            route: route.clone(),
        })
        .observe(started.elapsed().as_secs_f64());
    metrics
        .requests
        .get_or_create(&RequestLabels {
            route,
            method,
            status: response.status().as_u16(),
        })
        .inc();
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
struct InFlight<'a>(&'a Gauge);

impl<'a> InFlight<'a> {
    fn enter(gauge: &'a Gauge) -> Self {
        gauge.inc();
        Self(gauge)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.dec();
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
