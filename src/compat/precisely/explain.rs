//! `/v1/explain-addresscode`, a code Precisely's APIs return, read back as
//! words.

use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    bulk::{iso8601, now},
    error, given,
    match_code::{self, OfficialDescription},
    read_json,
};
use crate::{compat::VendorError, runtime::AppState};

/// Codes one request may carry.
const MAX_CODES: usize = 25;

const GLOBAL_MATCH_CODE: &str = "GlobalMatchCode";

/// Every code type Precisely explains. og issues only the Global Match
/// Code, so only it is read; the others name tables og does not hold.
const CODE_TYPES: [&str; 5] = [
    GLOBAL_MATCH_CODE,
    "GlobalLocationCode",
    "GlobalDeliveryIndicator",
    "ForwardResultCode",
    "ReverseResultCode",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExplainRequest {
    code_type: Option<String>,
    codes: Option<Vec<Item>>,
    #[serde(default)]
    response_options: Option<ResponseOptions>,
}

#[derive(Debug, Deserialize)]
struct Item {
    id: Option<String>,
    code: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseOptions {
    include_official_description: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Explained {
    responses: Vec<ItemResponse>,
    generated_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ExplainResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExplainResult {
    code_type: &'static str,
    customer_insight_summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    official_description: Option<OfficialDescription>,
}

const INVALID: &str = "INVALID_REQUEST";

fn explain(item: &Item, code_type: &'static str, official: bool) -> ItemResponse {
    let failed = |status, message: String| ItemResponse {
        id: item.id.clone(),
        status,
        result: None,
        error_message: Some(message),
    };
    let Some(code) = given(item.code.as_deref()) else {
        return failed(INVALID, "Code is null or blank (DIS-LI-1039)".to_string());
    };
    if code_type != GLOBAL_MATCH_CODE {
        return failed(
            "ERROR",
            format!(
                "{code_type} is not explained by this server, which issues Global Match Codes only"
            ),
        );
    }
    match match_code::parse(code) {
        Ok(parsed) => ItemResponse {
            id: item.id.clone(),
            status: "OK",
            result: Some(ExplainResult {
                code_type,
                customer_insight_summary: parsed.summary(),
                official_description: official.then(|| parsed.official()),
            }),
            error_message: None,
        },
        Err(detail) => failed(
            INVALID,
            format!("Invalid code format for {code_type} in the request (DIS-LI-1040): {detail}"),
        ),
    }
}

pub(super) async fn explain_addresscode(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, VendorError> {
    let body = read_json::<ExplainRequest>(&state, request, INVALID).await?;
    let invalid = |message: &str| error(StatusCode::BAD_REQUEST, INVALID, message);
    let named = given(body.code_type.as_deref())
        .ok_or_else(|| invalid("CodeType is not provided in the request (DIS-LI-1037)"))?;
    let code_type = CODE_TYPES
        .into_iter()
        .find(|known| *known == named)
        .ok_or_else(|| invalid("Invalid CodeType, it must be a valid codeType (DIS-LI-1038)"))?;
    let codes = body
        .codes
        .filter(|codes| !codes.is_empty())
        .ok_or_else(|| invalid("Request Body is not valid (DIS-LI-1018)"))?;
    if codes.len() > MAX_CODES {
        return Err(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "BATCH_SIZE_EXCEEDED",
            format!("Batch size must not exceed {MAX_CODES} (DIS-LI-1014)"),
        ));
    }
    let official = body
        .response_options
        .and_then(|options| options.include_official_description)
        .unwrap_or(false);
    let responses = codes
        .into_iter()
        .map(|item| explain(&item, code_type, official))
        .collect::<Vec<_>>();
    // Every code refused as malformed fails the request, with each reason.
    if responses.iter().all(|response| response.status == INVALID) {
        let errors = responses
            .iter()
            .map(|response| json!({"status": INVALID, "errorMessage": response.error_message}))
            .collect::<Vec<_>>();
        return Err(VendorError::new(
            StatusCode::BAD_REQUEST,
            json!({ "errors": errors }),
        ));
    }
    let status = if responses.iter().all(|response| response.status == "OK") {
        StatusCode::OK
    } else {
        StatusCode::PARTIAL_CONTENT
    };
    let millis = u64::try_from(now().as_millis()).unwrap_or(u64::MAX);
    Ok((
        status,
        Json(Explained {
            responses,
            generated_at: iso8601(millis),
        }),
    )
        .into_response())
}
