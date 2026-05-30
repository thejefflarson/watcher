//! Query API consumed by the UI. Runtime-checked sqlx queries (no DB needed at build time).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

type ApiError = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[derive(Deserialize)]
pub struct TraceQuery {
    limit: Option<i64>,
    service: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct TraceSummary {
    trace_id: String,
    service: Option<String>,
    root_name: Option<String>,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    duration_ms: f64,
    span_count: i64,
    error_count: i64,
}

/// GET /api/traces — recent traces, one row per trace_id.
pub async fn list_traces(
    State(pool): State<PgPool>,
    Query(q): Query<TraceQuery>,
) -> Result<Json<Vec<TraceSummary>>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let rows = sqlx::query_as::<_, TraceSummary>(
        "SELECT trace_id,
                max(service)                                                   AS service,
                (array_agg(name ORDER BY start_time))[1]                       AS root_name,
                min(start_time)                                                AS start_time,
                max(end_time)                                                  AS end_time,
                -- ::float8 because Postgres 14+ extract() returns numeric, which sqlx won't decode into f64
                (extract(epoch FROM (max(end_time) - min(start_time))) * 1000.0)::float8 AS duration_ms,
                count(*)                                                       AS span_count,
                count(*) FILTER (WHERE status_code = 2)                        AS error_count
         FROM spans
         WHERE ($1::text IS NULL OR service = $1)
         GROUP BY trace_id
         ORDER BY start_time DESC
         LIMIT $2",
    )
    .bind(q.service)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct SpanRow {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    service: Option<String>,
    name: String,
    kind: Option<i32>,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    duration_ms: f64,
    status_code: Option<i32>,
    status_message: Option<String>,
    attributes: serde_json::Value,
}

/// GET /api/traces/{trace_id} — all spans of a trace, ordered for waterfall rendering.
pub async fn get_trace(
    State(pool): State<PgPool>,
    Path(trace_id): Path<String>,
) -> Result<Json<Vec<SpanRow>>, ApiError> {
    let rows = sqlx::query_as::<_, SpanRow>(
        "SELECT trace_id, span_id, parent_span_id, service, name, kind,
                start_time, end_time, duration_ms, status_code, status_message, attributes
         FROM spans
         WHERE trace_id = $1
         ORDER BY start_time ASC",
    )
    .bind(trace_id)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct LogQuery {
    limit: Option<i64>,
    service: Option<String>,
    trace_id: Option<String>,
    q: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LogRow {
    id: i64,
    time: DateTime<Utc>,
    trace_id: Option<String>,
    span_id: Option<String>,
    service: Option<String>,
    severity_number: Option<i32>,
    severity_text: Option<String>,
    body: Option<String>,
    attributes: serde_json::Value,
}

/// GET /api/logs — recent logs with optional service / trace_id / full-text filters.
pub async fn list_logs(
    State(pool): State<PgPool>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Vec<LogRow>>, ApiError> {
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let rows = sqlx::query_as::<_, LogRow>(
        "SELECT id, time, trace_id, span_id, service, severity_number, severity_text, body, attributes
         FROM logs
         WHERE ($1::text IS NULL OR service = $1)
           AND ($2::text IS NULL OR trace_id = $2)
           AND ($3::text IS NULL OR body ILIKE '%' || $3 || '%')
         ORDER BY time DESC
         LIMIT $4",
    )
    .bind(q.service)
    .bind(q.trace_id)
    .bind(q.q)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}
