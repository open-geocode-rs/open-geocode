//! `POST /auth/v2/token`, the one stub among og's vendor routes. Precisely's
//! clients fetch a token before any other call, so og hands one out; it never
//! reads the Basic credentials, and no route checks the token it returns.

use axum::{Json, body::Bytes, http::StatusCode};
use serde::Serialize;
use serde_json::json;

use crate::compat::VendorError;

/// Seconds a Precisely token lasts. og's never expires, but a client
/// renews on this clock.
const EXPIRES_IN_SECONDS: u32 = 3600;

#[derive(Debug, Serialize)]
pub(super) struct Token {
    access_token: String,
    token_type: &'static str,
    expires_in: u32,
    scope: &'static str,
}

/// The token call's errors use OAuth's shape, not the API's envelope.
fn oauth_error(error: &str, description: &str) -> VendorError {
    VendorError::new(
        StatusCode::BAD_REQUEST,
        json!({"error": error, "error_description": description}),
    )
}

pub(super) async fn token(body: Bytes) -> Result<Json<Token>, VendorError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(oauth_error(
            "invalid_request",
            "Request body is not specified (DIS-AUTH-1019)",
        ));
    }
    let form = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&body)
        .map_err(|_| oauth_error("invalid_request", "Invalid request body (DIS-AUTH-1018)"))?;
    let field = |name: &str| {
        form.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if field("grant_type") != Some("client_credentials") {
        return Err(oauth_error(
            "unsupported_grant_type",
            "Invalid grant_type (DIS-AUTH-1016)",
        ));
    }
    // `scope` is required, with `default` as its default and only value.
    if field("scope").is_some_and(|scope| scope != "default") {
        return Err(oauth_error(
            "invalid_scope",
            "Invalid scope value (DIS-AUTH-1021)",
        ));
    }
    Ok(Json(Token {
        access_token: format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
        token_type: "Bearer",
        expires_in: EXPIRES_IN_SECONDS,
        scope: "default",
    }))
}
