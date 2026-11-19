//! `GET /v7/openapi`, the Batch API as this server serves it.
//!
//! The document lists only what og answers, so a client generated from it
//! calls nothing og lacks. It is written here, not copied, and links point
//! at the server that was asked.

use axum::{
    extract::OriginalUri,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::batch::{FILTERS, GEOCODE_HRN, Links, REVGEOCODE_HRN};

pub(super) async fn openapi(OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    let root = Links::of(&headers, &uri).root().to_string();
    let body = document(&root).to_string();
    // JSON is a subset of YAML 1.2, so the one text answers both. Only the
    // media type follows `Accept`, as a YAML client checks it.
    let yaml = crate::compat::header(&headers, "accept").and_then(|accept| {
        accept
            .split(',')
            .map(|range| range.split(';').next().unwrap_or("").trim())
            .find(|media| {
                matches!(
                    *media,
                    "text/yaml" | "application/vnd.oai.openapi" | "application/openapi+yaml"
                )
            })
    });
    let media_type = yaml.unwrap_or("application/json").to_string();
    let mut response = body.into_response();
    if let Ok(value) = HeaderValue::from_str(&media_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

fn reference(schema: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{schema}") })
}

fn json_body(schema: &str, description: &str) -> Value {
    json!({
        "description": description,
        "content": { "application/json": { "schema": reference(schema) } },
    })
}

fn error() -> Value {
    json_body("ErrorResponse", "The request failed.")
}

fn path_param(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "in": "path",
        "required": true,
        "description": description,
        "schema": { "type": "string", "minLength": 1 },
    })
}

fn query(name: &str, description: &str, schema: &Value) -> Value {
    json!({ "name": name, "in": "query", "description": description, "schema": schema })
}

fn operation(id: &str, summary: &str, parameters: &[Value], responses: &Value) -> Value {
    json!({
        "operationId": id,
        "summary": summary,
        "parameters": parameters,
        "responses": responses,
    })
}

fn with_body(mut operation: Value, body: Value) -> Value {
    operation["requestBody"] = body;
    operation
}

fn document(root: &str) -> Value {
    let job_id = path_param("jobId", "The ID the job was created with.");
    let notification_id = path_param(
        "notificationId",
        "The ID the notification was created with.",
    );
    let string = json!({ "type": "string" });
    let filter = json!({ "type": "string", "enum": FILTERS, "default": "any" });
    let job =
        |summary: &str| json!({ "200": json_body("JobResponse", summary), "default": error() });
    let file = |what: &str| {
        json!({
            "200": {
                "description": format!("The {what}."),
                "content": { "text/plain": { "schema": { "type": "string" } } },
            },
            "204": { "description": format!("The job has no {what}.") },
            "default": error(),
        })
    };
    let notification_body = json!({
        "required": true,
        "content": { "application/json": { "schema": reference("Notification") } },
    });
    let no_content = |what: &str| json!({ "204": { "description": what }, "default": error() });

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Batch API v7",
            "description": "Batch geocoding and reverse geocoding as open-geocode serves it under /compat/here.",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "servers": [{ "url": root }],
        "paths": {
            "/batch/jobs": {
                "post": with_body(
                    operation(
                        "postJob",
                        "Creates a job",
                        &[
                            query("serviceHrn", "The service the job runs.", &json!({
                                "type": "string",
                                "enum": [GEOCODE_HRN, REVGEOCODE_HRN],
                                "default": GEOCODE_HRN,
                            })),
                            query("tag", "A name for the job.", &string),
                            query("billingTags", "Up to 6 tags, joined by + or a comma.", &string),
                            query("inputDelimiter", "The input delimiter.", &json!({"type": "string", "default": "|"})),
                            query("outputDelimiter", "The output delimiter.", &json!({"type": "string", "default": "|"})),
                            query("inputColumns", "The input header, when the input has none.", &json!({"type": "string", "maxLength": 1024})),
                            query("outputColumns", "The CSV output columns.", &json!({"type": "string", "maxLength": 1024})),
                            query("outputFilter", "The JSONL fields to keep.", &json!({"type": "string", "maxLength": 1024})),
                            query("startJob", "Whether the job starts at once.", &json!({"type": "boolean", "default": true})),
                            query("outputType", "The output format.", &json!({"type": "string", "enum": ["csv", "jsonl"], "default": "csv"})),
                            query("notificationIds", "Up to 5 notification IDs, comma-separated, called when the job ends.", &string),
                        ],
                        &json!({ "201": json_body("JobResponse", "The job was created."), "default": error() }),
                    ),
                    json!({
                        "required": true,
                        "content": { "text/plain": { "schema": { "type": "string" } } },
                    }),
                ),
                "get": operation(
                    "getJobs",
                    "Lists jobs",
                    &[
                        query("status", "Only jobs in this status.", &filter),
                        query("tag", "Only jobs with this tag.", &string),
                        query("limit", "The most jobs per page.", &json!({"type": "integer", "minimum": 1, "maximum": 100, "default": 100})),
                        query("offset", "Jobs to skip.", &json!({"type": "integer", "minimum": 0, "default": 0})),
                    ],
                    &json!({ "200": json_body("PaginatedJobDetailResponse", "A page of jobs."), "default": error() }),
                ),
                "delete": operation(
                    "deleteJobs",
                    "Deletes jobs",
                    &[
                        json!({ "name": "status", "in": "query", "required": true, "description": "Only jobs in this status.", "schema": filter }),
                        query("tag", "Only jobs with this tag.", &string),
                    ],
                    &json!({ "200": json_body("JobsDeletedResponse", "The jobs deleted."), "default": error() }),
                ),
            },
            "/batch/jobs/{jobId}": {
                "get": operation(
                    "getJob",
                    "Gets a job",
                    std::slice::from_ref(&job_id),
                    &json!({ "200": json_body("JobDetailResponse", "The job."), "default": error() }),
                ),
                "delete": operation("deleteJob", "Deletes a job", std::slice::from_ref(&job_id), &job("The deleted job.")),
            },
            "/batch/jobs/{jobId}/start": {
                "put": operation("startJob", "Starts or resumes a job", std::slice::from_ref(&job_id), &job("The job.")),
            },
            "/batch/jobs/{jobId}/stop": {
                "put": operation("stopJob", "Stops a job", std::slice::from_ref(&job_id), &job("The job.")),
            },
            "/batch/jobs/{jobId}/results": {
                "get": operation("getJobResults", "Downloads the results", std::slice::from_ref(&job_id), &file("results")),
            },
            "/batch/jobs/{jobId}/errors": {
                "get": operation("getJobErrors", "Downloads the errors", std::slice::from_ref(&job_id), &file("errors")),
            },
            "/batch/notifications": {
                "post": with_body(
                    operation(
                        "postNotification",
                        "Creates a notification",
                        &[],
                        &json!({ "201": json_body("Notification", "The notification."), "default": error() }),
                    ),
                    notification_body.clone(),
                ),
                "get": operation(
                    "getNotifications",
                    "Lists notifications",
                    &[],
                    &json!({
                        "200": {
                            "description": "Every notification.",
                            "content": { "application/json": { "schema": { "type": "array", "items": reference("Notification") } } },
                        },
                        "default": error(),
                    }),
                ),
            },
            "/batch/notifications/{notificationId}": {
                "get": operation(
                    "getNotification",
                    "Gets a notification",
                    std::slice::from_ref(&notification_id),
                    &json!({ "200": json_body("Notification", "The notification."), "default": error() }),
                ),
                "put": with_body(
                    operation(
                        "putNotification",
                        "Replaces a notification",
                        std::slice::from_ref(&notification_id),
                        &json!({ "200": json_body("Notification", "The notification."), "default": error() }),
                    ),
                    notification_body,
                ),
                "delete": operation(
                    "deleteNotification",
                    "Deletes a notification",
                    std::slice::from_ref(&notification_id),
                    &no_content("The notification was deleted."),
                ),
            },
            "/batch/notifications/{notificationId}/test": {
                "get": operation(
                    "testNotification",
                    "Calls a notification with a made-up job",
                    std::slice::from_ref(&notification_id),
                    &no_content("The notification's endpoint took the call."),
                ),
            },
            "/version": {
                "get": operation("getVersion", "Gets the version", &[], &json!({ "200": json_body("VersionResponse", "The version.") })),
            },
            "/health": {
                "get": operation("getHealth", "Gets the health", &[], &json!({ "200": json_body("HealthResponse", "The health.") })),
            },
            "/openapi": {
                "get": operation(
                    "getOpenApi",
                    "Gets this document",
                    &[],
                    &json!({
                        "200": {
                            "description": "This document, as JSON, which is also YAML.",
                            "content": {
                                "application/json": { "schema": { "type": "string" } },
                                "text/yaml": { "schema": { "type": "string" } },
                            },
                        },
                    }),
                ),
            },
        },
        "components": { "schemas": schemas() },
    })
}

fn schemas() -> Value {
    let string = json!({ "type": "string" });
    let count = json!({ "type": "integer", "minimum": 0 });
    let time = json!({ "type": "string", "format": "date-time" });
    let status = json!({
        "type": "string",
        "enum": ["submitted", "inProgress", "completed", "stopped", "failure", "deleted"],
    });
    let job = json!({
        "id": string,
        "serviceHrn": string,
        "tag": string,
        "billingTags": { "type": "array", "items": string },
        "status": status,
        "href": string,
    });
    let mut detail = job.clone();
    for (name, value) in [
        ("created", time.clone()),
        ("started", time.clone()),
        ("ended", time),
        ("records", reference("JobRecords")),
        ("resultsHref", string.clone()),
        ("errorsHref", string.clone()),
        (
            "outputType",
            json!({ "type": "string", "enum": ["csv", "jsonl"] }),
        ),
        (
            "notificationIds",
            json!({ "type": "array", "items": string }),
        ),
    ] {
        detail[name] = value;
    }
    json!({
        "ErrorResponse": {
            "type": "object",
            "required": ["title", "status", "code", "cause", "action", "correlationId"],
            "properties": {
                "title": string,
                "status": { "type": "integer" },
                "code": string,
                "cause": string,
                "action": string,
                "correlationId": string,
            },
        },
        "JobResponse": { "type": "object", "required": ["id", "serviceHrn", "status"], "properties": job },
        "JobDetailResponse": { "type": "object", "required": ["id", "serviceHrn", "status"], "properties": detail },
        "JobRecords": {
            "type": "object",
            "properties": {
                "total": count, "valid": count, "invalid": count, "processed": count,
                "pending": count, "succeeded": count, "failed": count,
            },
        },
        "PaginatedJobDetailResponse": {
            "type": "object",
            "required": ["count"],
            "properties": {
                "limit": { "type": "integer" },
                "count": { "type": "integer" },
                "items": { "type": "array", "items": reference("JobDetailResponse") },
            },
        },
        "JobsDeletedResponse": {
            "type": "object",
            "required": ["total", "jobIds"],
            "properties": {
                "total": { "type": "integer" },
                "jobIds": { "type": "array", "items": string },
            },
        },
        "Notification": {
            "type": "object",
            "required": ["config"],
            "properties": {
                "id": { "type": "string", "readOnly": true },
                "type": { "type": "string", "enum": ["webHook"], "default": "webHook" },
                "lastUsedTime": { "type": "string", "format": "date-time", "readOnly": true },
                "config": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["url"],
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Called with GET when a job ends. ${JOB_ID}, ${JOB_NAME}, ${JOB_STATUS}, ${JOB_OUTPUT_TYPE}, ${JOB_RECORDS_SUCCEEDED}, ${JOB_RECORDS_FAILED}, ${JOB_RECORDS_TOTAL}, ${JOB_RECORDS_VALID} and ${JOB_RECORDS_INVALID} in it are filled in.",
                        },
                    },
                },
            },
        },
        "VersionResponse": {
            "type": "object",
            "required": ["apiVersion", "serviceVersion"],
            "properties": { "apiVersion": string, "serviceVersion": string },
        },
        "HealthResponse": {
            "type": "object",
            "properties": { "status": { "type": "string", "enum": ["ok", "fail"] } },
        },
    })
}
