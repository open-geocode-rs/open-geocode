//! `POST /v1/signals`, what an end user did with a result.
//!
//! og ranks from its Pack alone and keeps no feedback store, so a valid
//! signal is written to the log as one JSON line and answered as HERE
//! answers it. An operator who wants the signals reads them there.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value, json};

use crate::runtime::AppState;

use super::{Gs7Error, Rid, media_type, supported};

const ACTIONS: [&str; 9] = [
    "here:gs:action:call",
    "here:gs:action:email",
    "here:gs:action:navigate-from",
    "here:gs:action:navigate-to",
    "here:gs:action:pin",
    "here:gs:action:tap",
    "here:gs:action:view",
    "here:gs:action:share",
    "here:gs:action:visit",
];

/// One line of the body, checked.
#[derive(Debug, PartialEq, Eq)]
struct Signal {
    action: String,
    correlation_id: String,
    resource_id: String,
    rank: u64,
    timestamp: Option<u64>,
}

pub(super) async fn signals(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match read(&headers, &body) {
        Ok(signals) => {
            for signal in signals {
                state.error_log().event(&json!({
                    "level": "info",
                    "message": "here signal",
                    "request_id": request_id,
                    "action": signal.action,
                    "correlation_id": signal.correlation_id,
                    "resource_id": signal.resource_id,
                    "rank": signal.rank,
                    "timestamp": signal.timestamp,
                }));
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => error.into_vendor(&request_id).into_response(),
    }
}

fn read(headers: &HeaderMap, body: &[u8]) -> Result<Vec<Signal>, Gs7Error> {
    let accepted = media_type(headers).is_some_and(|media_type| {
        media_type == "text/plain" || media_type == "application/x-www-form-urlencoded"
    });
    if !accepted {
        return Err(
            Gs7Error::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported Media Type")
                .action("Send the body as text/plain or application/x-www-form-urlencoded"),
        );
    }
    let body = std::str::from_utf8(body).map_err(|_| {
        Gs7Error::new(StatusCode::BAD_REQUEST, "Malformed request body")
            .cause("The body is not UTF-8")
    })?;
    let signals = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(signal)
        .collect::<Result<Vec<_>, _>>()?;
    if signals.is_empty() {
        return Err(
            Gs7Error::new(StatusCode::BAD_REQUEST, "Empty request body").action(
                "Send one signal per line, such as version=1&action=here:gs:action:view&correlationId={id}&resourceId={id}&rank=0",
            ),
        );
    }
    Ok(signals)
}

fn signal(line: &str) -> Result<Signal, Gs7Error> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(line).map_err(|_| {
        Gs7Error::new(StatusCode::BAD_REQUEST, "Malformed signal")
            .cause("A line is not URL-encoded key=value pairs")
    })?;
    let mut fields = Map::new();
    for (name, value) in pairs {
        if fields.contains_key(&name) {
            return Err(Gs7Error::new(
                StatusCode::BAD_REQUEST,
                format!("Duplicate parameter '{name}'"),
            )
            .action("Send each parameter once"));
        }
        fields.insert(name, Value::String(value.trim().to_string()));
    }
    let field = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let required = |name: &'static str| {
        field(name).ok_or_else(|| {
            Gs7Error::new(
                StatusCode::BAD_REQUEST,
                format!("Missing parameter '{name}'"),
            )
            .action(format!("Provide '{name}'"))
        })
    };
    let number = |name: &'static str, value: &str| {
        value
            .parse::<u64>()
            .map_err(|_| Gs7Error::illegal(name, value, "Provide a non-negative integer"))
    };

    let version = required("version")?;
    if version != "1" {
        return Err(Gs7Error::illegal(
            "version",
            version,
            supported(version, &["1"]),
        ));
    }
    let action = required("action")?;
    if !ACTIONS.contains(&action) {
        return Err(Gs7Error::illegal(
            "action",
            action,
            supported(action, &ACTIONS),
        ));
    }
    let rank = number("rank", required("rank")?)?;
    let timestamp = field("timestamp")
        .map(|value| number("timestamp", value))
        .transpose()?;
    Ok(Signal {
        action: action.to_string(),
        correlation_id: required("correlationId")?.to_string(),
        resource_id: required("resourceId")?.to_string(),
        rank,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: &str = "version=1&action=here:gs:action:view&correlationId=c-1&resourceId=r-1&rank=2&timestamp=1692625661";

    fn text() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "text/plain; charset=UTF-8".parse().expect("header"),
        );
        headers
    }

    fn title(line: &str) -> String {
        read(&text(), line.as_bytes()).expect_err("refused").title
    }

    #[test]
    fn a_signal_line_reads_every_field() {
        let body = format!(
            "{VIEW}\n\nversion=1&action=here:gs:action:call&correlationId=c-2&resourceId=r-2&rank=0\n"
        );
        assert_eq!(
            read(&text(), body.as_bytes()).expect("read"),
            [
                Signal {
                    action: "here:gs:action:view".into(),
                    correlation_id: "c-1".into(),
                    resource_id: "r-1".into(),
                    rank: 2,
                    timestamp: Some(1_692_625_661),
                },
                Signal {
                    action: "here:gs:action:call".into(),
                    correlation_id: "c-2".into(),
                    resource_id: "r-2".into(),
                    rank: 0,
                    timestamp: None,
                },
            ]
        );
    }

    #[test]
    fn a_signal_is_refused_as_here_refuses_it() {
        assert_eq!(title(""), "Empty request body");
        assert_eq!(
            title(&VIEW.replace("version=1", "version=2")),
            "Illegal input for parameter 'version'"
        );
        assert_eq!(
            title(&VIEW.replace(":view", ":like")),
            "Illegal input for parameter 'action'"
        );
        assert_eq!(
            title(&VIEW.replace("rank=2", "rank=-1")),
            "Illegal input for parameter 'rank'"
        );
        assert_eq!(
            title(&VIEW.replace("&resourceId=r-1", "")),
            "Missing parameter 'resourceId'"
        );
        assert_eq!(
            title(&format!("{VIEW}&rank=3")),
            "Duplicate parameter 'rank'"
        );
        let json = HeaderMap::from_iter([(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().expect("header"),
        )]);
        let refused = read(&json, VIEW.as_bytes()).expect_err("refused");
        assert_eq!(refused.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
}
