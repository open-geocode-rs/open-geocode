use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use geojson::{Geometry, GeometryValue};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, task};
use tower_http::services::ServeDir;

use crate::{
    record::{
        DerivedSourceProvenance, LocationPrecision, NormalizedRecord, OsmObjectType,
        SourceProvenance,
    },
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
            id: hit.record.id().to_string(),
            layer: hit.record.layer().to_string(),
            label: hit.record.label().to_string(),
            score: hit.score,
            point: display_point(&hit.record),
            source: source_summary(&hit.record),
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

fn display_point(record: &NormalizedRecord) -> Option<SearchApiPoint> {
    match record {
        NormalizedRecord::Address(record) => point_from_geometry(
            &record.geometry,
            point_precision_from_location(record.location_precision()),
        ),
        NormalizedRecord::Country(record)
        | NormalizedRecord::District(record)
        | NormalizedRecord::Locality(record)
        | NormalizedRecord::Neighbourhood(record)
        | NormalizedRecord::Place(record)
        | NormalizedRecord::Region(record) => {
            point_from_geometry(&record.geometry, SearchApiPointPrecision::Point)
        }
        NormalizedRecord::Interpolation(record) => {
            Some(representative_point(record.representative_point))
        }
        NormalizedRecord::Postcode(record) => {
            point_from_geometry(&record.geometry, SearchApiPointPrecision::Point)
        }
        NormalizedRecord::Street(record) => Some(representative_point(record.representative_point)),
    }
}

fn point_from_geometry(
    geometry: &Geometry,
    precision: SearchApiPointPrecision,
) -> Option<SearchApiPoint> {
    let GeometryValue::Point { coordinates } = &geometry.value else {
        return None;
    };
    let [lon, lat, ..] = coordinates.as_slice() else {
        return None;
    };
    Some(SearchApiPoint {
        lon: *lon,
        lat: *lat,
        precision,
    })
}

fn representative_point(point: [f64; 2]) -> SearchApiPoint {
    SearchApiPoint {
        lon: point[0],
        lat: point[1],
        precision: SearchApiPointPrecision::RepresentativePoint,
    }
}

fn point_precision_from_location(precision: LocationPrecision) -> SearchApiPointPrecision {
    match precision {
        LocationPrecision::Point => SearchApiPointPrecision::Point,
        LocationPrecision::Centroid => SearchApiPointPrecision::Centroid,
    }
}

fn source_summary(record: &NormalizedRecord) -> SearchApiSource {
    match record {
        NormalizedRecord::Address(record) => source_from_osm(&record.source),
        NormalizedRecord::Country(record)
        | NormalizedRecord::District(record)
        | NormalizedRecord::Locality(record)
        | NormalizedRecord::Neighbourhood(record)
        | NormalizedRecord::Place(record)
        | NormalizedRecord::Region(record) => source_from_osm(&record.source),
        NormalizedRecord::Interpolation(record) => source_from_osm(&record.source),
        NormalizedRecord::Postcode(record) => source_from_derived(&record.source),
        NormalizedRecord::Street(record) => source_from_osm(&record.source),
    }
}

fn source_from_osm(source: &SourceProvenance) -> SearchApiSource {
    SearchApiSource {
        dataset: source.dataset.clone(),
        object_type: Some(source.object_type),
        object_id: Some(source.object_id),
        derived_from: None,
        record_count: None,
    }
}

fn source_from_derived(source: &DerivedSourceProvenance) -> SearchApiSource {
    SearchApiSource {
        dataset: source.dataset.clone(),
        object_type: None,
        object_id: None,
        derived_from: Some(source.derived_from.clone()),
        record_count: Some(source.record_count),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::record::{
        AddressComponents, AddressRecord, LocationPrecision, NormalizedRecord, OsmObjectType,
        SourceProvenance, StreetRecord, point_geometry,
    };

    use super::*;

    #[test]
    fn api_result_shapes_address_point() {
        let hit = TextSearchHit {
            record_id: 7,
            score: 4.25,
            record: NormalizedRecord::address(AddressRecord {
                id: "osm:node:1".to_string(),
                label: "10 King Street, Toronto".to_string(),
                name: "10 King Street".to_string(),
                address: AddressComponents {
                    number: "10".to_string(),
                    street: Some("King Street".to_string()),
                    place: None,
                    unit: None,
                    locality: Some("Toronto".to_string()),
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: point_geometry(-79.3832, 43.6532),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(OsmObjectType::Node, 1),
            }),
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
            record: NormalizedRecord::street(StreetRecord {
                id: "osm:way:9".to_string(),
                label: "King Street".to_string(),
                name: "King Street".to_string(),
                geometry: point_geometry(-79.4, 43.6),
                representative_point: [-79.41, 43.61],
                source: SourceProvenance {
                    dataset: "osm".to_string(),
                    object_type: OsmObjectType::Way,
                    object_id: 9,
                    tags: Some(BTreeMap::new()),
                },
            }),
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
