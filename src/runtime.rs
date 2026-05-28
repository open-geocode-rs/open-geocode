use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, task};
use tower_http::services::ServeDir;

use crate::{
    pack::{RecordPoint, RecordPointPrecision, RecordSource},
    record::OsmObjectType,
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions, ReverseGeocodeResponse},
    search::{PackTextSearcher, TextAutocompleteOptions, TextSearchHit, TextSearchOptions},
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub pack: PathBuf,
    pub demo: PathBuf,
    pub bind: SocketAddr,
}

#[derive(Clone)]
struct AppState {
    searcher: Arc<PackTextSearcher>,
    reverse_geocoder: Arc<PackReverseGeocoder>,
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    q: Option<String>,
    #[serde(default)]
    limit: usize,
    layer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReverseParams {
    lon: f64,
    lat: f64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SearchResponse {
    pub query: String,
    pub results: Vec<SearchApiResult>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AutocompleteResponse {
    pub query: String,
    pub suggestions: Vec<SearchApiResult>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SearchApiResult {
    pub record_id: u64,
    pub id: String,
    pub layer: String,
    pub label: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<SearchApiPoint>,
    pub source: SearchApiSource,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchApiPoint {
    pub lon: f64,
    pub lat: f64,
    pub precision: SearchApiPointPrecision,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchApiPointPrecision {
    Point,
    Centroid,
    RepresentativePoint,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SearchApiSource {
    pub dataset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_type: Option<OsmObjectType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_count: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let searcher = PackTextSearcher::open(&options.pack)
        .with_context(|| format!("failed to open Pack {}", options.pack.display()))?;
    let reverse_geocoder = PackReverseGeocoder::open(&options.pack).with_context(|| {
        format!(
            "failed to open Pack Spatial Index {}",
            options.pack.display()
        )
    })?;
    let state = AppState {
        searcher: Arc::new(searcher),
        reverse_geocoder: Arc::new(reverse_geocoder),
    };
    let app = Router::new()
        .route("/search", get(search))
        .route("/autocomplete", get(autocomplete))
        .route("/reverse", get(reverse))
        .fallback_service(ServeDir::new(&options.demo).append_index_html_on_directories(true))
        .with_state(state);

    let listener = TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("failed to bind {}", options.bind))?;
    println!(
        "Serving {} at http://{}",
        options.pack.display(),
        options.bind
    );
    axum::serve(listener, app)
        .await
        .context("runtime server failed")
}

async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let query = params.q.unwrap_or_default().trim().to_string();
    if query.is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }

    let options = TextSearchOptions {
        query: query.clone(),
        limit: params.limit,
        layer: params.layer,
    };
    let searcher = Arc::clone(&state.searcher);
    let hits = task::spawn_blocking(move || searcher.search(options))
        .await
        .map_err(|_| ApiError::internal("search task failed"))?
        .map_err(search_api_error)?;

    Ok(Json(SearchResponse {
        query,
        results: hits.into_iter().map(SearchApiResult::from_hit).collect(),
    }))
}

async fn autocomplete(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<AutocompleteResponse>, ApiError> {
    let query = params.q.unwrap_or_default().trim().to_string();
    let options = TextAutocompleteOptions {
        query: query.clone(),
        limit: params.limit,
        layer: params.layer,
    };
    let searcher = Arc::clone(&state.searcher);
    let hits = task::spawn_blocking(move || searcher.autocomplete(options))
        .await
        .map_err(|_| ApiError::internal("autocomplete task failed"))?
        .map_err(search_api_error)?;

    Ok(Json(AutocompleteResponse {
        query,
        suggestions: hits.into_iter().map(SearchApiResult::from_hit).collect(),
    }))
}

async fn reverse(
    State(state): State<AppState>,
    Query(params): Query<ReverseParams>,
) -> Result<Json<ReverseGeocodeResponse>, ApiError> {
    let geocoder = Arc::clone(&state.reverse_geocoder);
    let response = task::spawn_blocking(move || {
        geocoder.reverse(ReverseGeocodeOptions {
            lon: params.lon,
            lat: params.lat,
        })
    })
    .await
    .map_err(|_| ApiError::internal("reverse task failed"))?
    .map_err(|_| ApiError::internal("reverse failed"))?;

    Ok(Json(response))
}

impl SearchApiResult {
    fn from_hit(hit: TextSearchHit) -> Self {
        Self {
            record_id: hit.record_id,
            id: hit.record.id,
            layer: hit.record.layer,
            label: hit.record.label,
            score: hit.score,
            point: hit.record.point.map(SearchApiPoint::from),
            source: SearchApiSource::from(hit.record.source),
        }
    }
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

fn search_api_error(error: anyhow::Error) -> ApiError {
    let details = format!("{error:#}");
    if details.contains("failed to parse search query") {
        ApiError::bad_request("invalid search query")
    } else {
        ApiError::internal("search failed")
    }
}

impl From<RecordPoint> for SearchApiPoint {
    fn from(point: RecordPoint) -> Self {
        Self {
            lon: point.lon,
            lat: point.lat,
            precision: match point.precision {
                RecordPointPrecision::Point => SearchApiPointPrecision::Point,
                RecordPointPrecision::Centroid => SearchApiPointPrecision::Centroid,
                RecordPointPrecision::RepresentativePoint => {
                    SearchApiPointPrecision::RepresentativePoint
                }
            },
        }
    }
}

impl From<RecordSource> for SearchApiSource {
    fn from(source: RecordSource) -> Self {
        Self {
            dataset: source.dataset,
            object_type: source.object_type,
            object_id: source.object_id,
            derived_from: source.derived_from,
            record_count: source.record_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::pack::RecordSummary;
    use crate::record::OsmObjectType;

    use super::*;

    #[test]
    fn api_result_shapes_address_point() {
        let hit = TextSearchHit {
            record_id: 7,
            score: 4.25,
            record: RecordSummary {
                id: "osm:node:1".to_string(),
                layer: "address".to_string(),
                label: "10 King Street, Toronto".to_string(),
                point: Some(RecordPoint {
                    lon: -79.3832,
                    lat: 43.6532,
                    precision: RecordPointPrecision::Point,
                }),
                source: RecordSource {
                    dataset: "osm".to_string(),
                    object_type: Some(OsmObjectType::Node),
                    object_id: Some(1),
                    derived_from: None,
                    record_count: None,
                },
            },
        };

        let result = SearchApiResult::from_hit(hit);

        assert_eq!(result.record_id, 7);
        assert_eq!(result.id, "osm:node:1");
        assert_eq!(result.layer, "address");
        assert_eq!(result.label, "10 King Street, Toronto");
        assert_eq!(
            result.point,
            Some(SearchApiPoint {
                lon: -79.3832,
                lat: 43.6532,
                precision: SearchApiPointPrecision::Point,
            })
        );
        assert_eq!(result.source.dataset, "osm");
        assert_eq!(result.source.object_type, Some(OsmObjectType::Node));
    }

    #[test]
    fn api_result_uses_representative_point_for_street() {
        let hit = TextSearchHit {
            record_id: 12,
            score: 3.0,
            record: RecordSummary {
                id: "osm:way:9".to_string(),
                layer: "street".to_string(),
                label: "King Street".to_string(),
                point: Some(RecordPoint {
                    lon: -79.41,
                    lat: 43.61,
                    precision: RecordPointPrecision::RepresentativePoint,
                }),
                source: RecordSource {
                    dataset: "osm".to_string(),
                    object_type: Some(OsmObjectType::Way),
                    object_id: Some(9),
                    derived_from: None,
                    record_count: None,
                },
            },
        };

        let result = SearchApiResult::from_hit(hit);

        assert_eq!(
            result.point,
            Some(SearchApiPoint {
                lon: -79.41,
                lat: 43.61,
                precision: SearchApiPointPrecision::RepresentativePoint,
            })
        );
    }
}
