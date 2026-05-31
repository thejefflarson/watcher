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
    /// Time window (RFC3339); both optional. Absent ends are unbounded.
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
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
           AND ($2::timestamptz IS NULL OR start_time >= $2)
           AND ($3::timestamptz IS NULL OR start_time <= $3)
         GROUP BY trace_id
         ORDER BY start_time DESC
         LIMIT $4",
    )
    .bind(q.service)
    .bind(q.from)
    .bind(q.to)
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
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    /// Attribute equality filter, `key=value` (e.g. `k8s.pod.name=api-7f`).
    attr: Option<String>,
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
    // `key=value` → JSONB containment `attributes @> {"key":"value"}`, which the
    // logs_attrs_gin index serves.
    let attr_json = q
        .attr
        .as_deref()
        .and_then(|s| s.split_once('='))
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| serde_json::json!({ k: v }));
    let rows = sqlx::query_as::<_, LogRow>(
        "SELECT id, time, trace_id, span_id, service, severity_number, severity_text, body, attributes
         FROM logs
         WHERE ($1::text IS NULL OR service = $1)
           AND ($2::text IS NULL OR trace_id = $2)
           AND ($3::text IS NULL OR body ILIKE '%' || $3 || '%')
           AND ($4::timestamptz IS NULL OR time >= $4)
           AND ($5::timestamptz IS NULL OR time <= $5)
           AND ($6::jsonb IS NULL OR attributes @> $6)
         ORDER BY time DESC
         LIMIT $7",
    )
    .bind(q.service)
    .bind(q.trace_id)
    .bind(q.q)
    .bind(q.from)
    .bind(q.to)
    .bind(attr_json)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct MetricQuery {
    limit: Option<i64>,
    service: Option<String>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct MetricSummary {
    name: String,
    service: Option<String>,
    kind: Option<String>,
    unit: Option<String>,
    last_time: DateTime<Utc>,
    last_value: Option<f64>,
    /// Up to 30 most-recent values (newest first) for an inline sparkline.
    /// Each value is the sum across the metric's series at that tick, so a
    /// multi-series metric (e.g. container.cpu.time over many containers)
    /// reads as one coherent total rather than interleaved series.
    spark: Option<Vec<f64>>,
    /// Distinct series collapsed into this row (max points sharing a tick).
    /// 1 for a plain metric; >1 signals the per-label breakdown lives in the chart.
    series_count: Option<i64>,
}

/// GET /api/metrics — one row per metric series with its latest value.
pub async fn list_metrics(
    State(pool): State<PgPool>,
    Query(q): Query<MetricQuery>,
) -> Result<Json<Vec<MetricSummary>>, ApiError> {
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    // Reading every point in the window to GROUP BY name was ~minutes at scale.
    // Instead: enumerate distinct names with a loose index scan (recursive CTE),
    // then per name read only the 30 most-recent points (via metrics_name_time_idx)
    // for the latest value + sparkline. Names with no points in the window drop out.
    let rows = sqlx::query_as::<_, MetricSummary>(
        "WITH RECURSIVE names AS (
             SELECT min(name) AS name FROM metrics
             UNION ALL
             SELECT (SELECT min(name) FROM metrics WHERE name > n.name)
             FROM names n WHERE n.name IS NOT NULL
         )
         SELECT n.name, r.service, r.kind, r.unit, r.last_time, r.last_value,
                r.spark, r.series_count
         FROM names n
         CROSS JOIN LATERAL (
             -- Collapse a metric's many series into one coherent total trend:
             -- bucket time (120s), take each series' latest value per bucket,
             -- then sum across series. Without the per-series step, summing raw
             -- points interleaves series whose timestamps don't line up and the
             -- sparkline is noise. Bounded by LIMIT so the per-name read stays
             -- index-fast (metrics_name_time_idx) regardless of table size.
             SELECT (array_agg(service ORDER BY bucket DESC))[1] AS service,
                    (array_agg(kind    ORDER BY bucket DESC))[1] AS kind,
                    (array_agg(unit    ORDER BY bucket DESC))[1] AS unit,
                    (array_agg(v        ORDER BY bucket DESC))[1] AS last_value,
                    max(bucket)                                  AS last_time,
                    array_agg(v ORDER BY bucket DESC) FILTER (WHERE v IS NOT NULL) AS spark,
                    max(series)                                  AS series_count
             FROM (
                 SELECT bucket,
                        sum(sv)                                       AS v,
                        (array_agg(service ORDER BY sv_time DESC))[1] AS service,
                        (array_agg(kind    ORDER BY sv_time DESC))[1] AS kind,
                        (array_agg(unit    ORDER BY sv_time DESC))[1] AS unit,
                        count(*)                                      AS series
                 FROM (
                     SELECT bucket, service, attributes,
                            (array_agg(value ORDER BY time DESC))[1] AS sv,
                            (array_agg(kind  ORDER BY time DESC))[1] AS kind,
                            (array_agg(unit  ORDER BY time DESC))[1] AS unit,
                            max(time)                                AS sv_time
                     FROM (
                         SELECT to_timestamp(floor(extract(epoch FROM time) / 120) * 120) AS bucket,
                                service, kind, unit, attributes, value, time
                         FROM metrics
                         WHERE name = n.name
                           AND ($1::text IS NULL OR service = $1)
                           AND ($2::timestamptz IS NULL OR time >= $2)
                           AND ($3::timestamptz IS NULL OR time <= $3)
                         ORDER BY time DESC
                         LIMIT 5000
                     ) recent
                     GROUP BY bucket, service, attributes
                 ) per_series
                 GROUP BY bucket
                 ORDER BY bucket DESC
                 LIMIT 30
             ) ticks
         ) r
         WHERE n.name IS NOT NULL AND r.last_time IS NOT NULL
         ORDER BY n.name
         LIMIT $4",
    )
    .bind(q.service)
    .bind(q.from)
    .bind(q.to)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

/// Time-bucket width (seconds) used for rollups and raw-series bucketing.
/// Matches rollup.rs' `WATCHER_ROLLUP_BUCKET_SECS` so series points line up.
fn rollup_bucket_secs() -> f64 {
    std::env::var("WATCHER_ROLLUP_BUCKET_SECS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(300.0)
}

#[derive(Deserialize)]
pub struct SeriesQuery {
    name: String,
    service: Option<String>,
    hours: Option<i32>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct SeriesPoint {
    t: DateTime<Utc>,
    v: Option<f64>,
}

/// GET /api/metrics/series — a time series for one metric, stitched from
/// rollups (older buckets) and raw points (newer than the last rollup bucket),
/// so it stays continuous after raw points are pruned. Bucket-average values.
pub async fn metric_series(
    State(pool): State<PgPool>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<Vec<SeriesPoint>>, ApiError> {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 90);
    let width = rollup_bucket_secs();
    let rows = sqlx::query_as::<_, SeriesPoint>(
        "WITH last_roll AS (
             SELECT max(bucket) AS b
             FROM metric_rollups
             WHERE name = $1 AND ($2::text IS NULL OR service = $2)
         )
         SELECT bucket AS t, avg AS v
         FROM metric_rollups
         WHERE name = $1 AND ($2::text IS NULL OR service = $2)
           AND bucket >= now() - make_interval(hours => $3)
         UNION ALL
         SELECT to_timestamp(floor(extract(epoch FROM time)::float8 / $4) * $4) AS t,
                avg(value) AS v
         FROM metrics, last_roll
         WHERE name = $1 AND ($2::text IS NULL OR service = $2)
           AND time >= now() - make_interval(hours => $3)
           AND (last_roll.b IS NULL OR time >= last_roll.b + make_interval(secs => $4))
         GROUP BY t
         ORDER BY t ASC",
    )
    .bind(q.name)
    .bind(q.service)
    .bind(hours)
    .bind(width)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct DimsQuery {
    name: String,
}

/// GET /api/metrics/dims — attribute keys a metric can be grouped by
/// (e.g. k8s.pod.name, k8s.node.name, k8s.container.name).
pub async fn metric_dims(
    State(pool): State<PgPool>,
    Query(q): Query<DimsQuery>,
) -> Result<Json<Vec<String>>, ApiError> {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT key
         FROM metrics, jsonb_object_keys(attributes) AS key
         WHERE name = $1 AND time >= now() - interval '2 days'
         ORDER BY key",
    )
    .bind(q.name)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(keys))
}

#[derive(Deserialize)]
pub struct GroupedQuery {
    name: String,
    group_by: String,
    hours: Option<i32>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LabeledPoint {
    label: Option<String>,
    t: DateTime<Utc>,
    v: Option<f64>,
}

/// GET /api/metrics/series_grouped — one bucketed series per distinct value of
/// `group_by` (e.g. one line per pod). Raw points only, since rollups aggregate
/// the dimensions away — so this covers the raw-retention window.
pub async fn metric_series_grouped(
    State(pool): State<PgPool>,
    Query(q): Query<GroupedQuery>,
) -> Result<Json<Vec<LabeledPoint>>, ApiError> {
    let hours = q.hours.unwrap_or(6).clamp(1, 24 * 7);
    let width = rollup_bucket_secs();
    let rows = sqlx::query_as::<_, LabeledPoint>(
        "SELECT attributes->>$2 AS label,
                to_timestamp(floor(extract(epoch FROM time)::float8 / $4) * $4) AS t,
                avg(value) AS v
         FROM metrics
         WHERE name = $1 AND attributes ? $2
           AND time >= now() - make_interval(hours => $3)
         GROUP BY label, t
         ORDER BY label NULLS LAST, t ASC",
    )
    .bind(&q.name)
    .bind(&q.group_by)
    .bind(hours)
    .bind(width)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct RedQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ServiceRed {
    service: String,
    spans: i64,
    errors: i64,
    error_rate: f64,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    p99_ms: Option<f64>,
}

/// GET /api/services — RED (Rate, Errors, Duration) per service over a window:
/// span count, error count + rate, and latency p50/p95/p99.
pub async fn service_red(
    State(pool): State<PgPool>,
    Query(q): Query<RedQuery>,
) -> Result<Json<Vec<ServiceRed>>, ApiError> {
    let rows = sqlx::query_as::<_, ServiceRed>(
        "SELECT service,
                count(*)                                AS spans,
                count(*) FILTER (WHERE status_code = 2) AS errors,
                (count(*) FILTER (WHERE status_code = 2))::float8
                    / nullif(count(*), 0)::float8       AS error_rate,
                percentile_cont(0.5)  WITHIN GROUP (ORDER BY duration_ms) AS p50_ms,
                percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95_ms,
                percentile_cont(0.99) WITHIN GROUP (ORDER BY duration_ms) AS p99_ms
         FROM spans
         WHERE service IS NOT NULL
           AND ($1::timestamptz IS NULL OR start_time >= $1)
           AND ($2::timestamptz IS NULL OR start_time <= $2)
         GROUP BY service
         ORDER BY spans DESC",
    )
    .bind(q.from)
    .bind(q.to)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Serialize, sqlx::FromRow)]
struct ServiceEdge {
    source: String,
    target: String,
    calls: i64,
}

#[derive(Serialize)]
pub struct ServiceMap {
    nodes: Vec<String>,
    edges: Vec<ServiceEdge>,
}

/// GET /api/servicemap — service dependency graph derived from span parent/child links.
pub async fn service_map(State(pool): State<PgPool>) -> Result<Json<ServiceMap>, ApiError> {
    let nodes: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT service FROM spans WHERE service IS NOT NULL ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .map_err(internal)?;

    let edges = sqlx::query_as::<_, ServiceEdge>(
        "SELECT parent.service AS source, child.service AS target, count(*) AS calls
         FROM spans child
         JOIN spans parent
           ON child.parent_span_id = parent.span_id
          AND child.trace_id = parent.trace_id
         WHERE parent.service IS NOT NULL
           AND child.service IS NOT NULL
           AND parent.service IS DISTINCT FROM child.service
         GROUP BY parent.service, child.service
         ORDER BY calls DESC",
    )
    .fetch_all(&pool)
    .await
    .map_err(internal)?;

    Ok(Json(ServiceMap { nodes, edges }))
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AlertRuleView {
    id: i64,
    name: String,
    metric: String,
    service: Option<String>,
    comparator: String,
    threshold: f64,
    agg: String,
    window_secs: i32,
    enabled: bool,
    created_at: DateTime<Utc>,
    /// True when the rule has an unresolved (open) event.
    firing: bool,
}

/// GET /api/alerts — all rules with their current firing state.
pub async fn list_alerts(State(pool): State<PgPool>) -> Result<Json<Vec<AlertRuleView>>, ApiError> {
    let rows = sqlx::query_as::<_, AlertRuleView>(
        "SELECT r.id, r.name, r.metric, r.service, r.comparator, r.threshold, r.agg,
                r.window_secs, r.enabled, r.created_at,
                (e.id IS NOT NULL) AS firing
         FROM alert_rules r
         LEFT JOIN alert_events e ON e.rule_id = r.id AND e.resolved_at IS NULL
         ORDER BY r.created_at DESC",
    )
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct NewAlertRule {
    name: String,
    metric: String,
    service: Option<String>,
    comparator: String,
    threshold: f64,
    agg: Option<String>,
    window_secs: Option<i32>,
}

/// POST /api/alerts — create a rule. Validates the enum-ish fields so the
/// evaluator's whitelisted SQL never sees anything unexpected.
pub async fn create_alert(
    State(pool): State<PgPool>,
    Json(body): Json<NewAlertRule>,
) -> Result<Json<i64>, ApiError> {
    if body.name.trim().is_empty() || body.metric.trim().is_empty() {
        return Err(bad_request("name and metric are required"));
    }
    if !matches!(body.comparator.as_str(), "gt" | "lt") {
        return Err(bad_request("comparator must be 'gt' or 'lt'"));
    }
    let agg = body.agg.unwrap_or_else(|| "avg".to_string());
    if !matches!(agg.as_str(), "avg" | "max" | "min" | "sum" | "last") {
        return Err(bad_request("agg must be one of avg|max|min|sum|last"));
    }
    let window_secs = body.window_secs.unwrap_or(300).clamp(10, 86_400);
    let service = body.service.filter(|s| !s.is_empty());

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO alert_rules (name, metric, service, comparator, threshold, agg, window_secs)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(body.name)
    .bind(body.metric)
    .bind(service)
    .bind(body.comparator)
    .bind(body.threshold)
    .bind(agg)
    .bind(window_secs)
    .fetch_one(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(id))
}

/// DELETE /api/alerts/{id} — remove a rule (its events cascade).
pub async fn delete_alert(
    State(pool): State<PgPool>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let res = sqlx::query("DELETE FROM alert_rules WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .map_err(internal)?;
    if res.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "no such rule".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct EventQuery {
    limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AlertEventView {
    id: i64,
    rule_id: i64,
    rule_name: String,
    metric: String,
    value: Option<f64>,
    fired_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
}

/// GET /api/alerts/events — recent firing/resolved transitions, newest first.
pub async fn list_alert_events(
    State(pool): State<PgPool>,
    Query(q): Query<EventQuery>,
) -> Result<Json<Vec<AlertEventView>>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let rows = sqlx::query_as::<_, AlertEventView>(
        "SELECT e.id, e.rule_id, r.name AS rule_name, r.metric, e.value, e.fired_at, e.resolved_at
         FROM alert_events e
         JOIN alert_rules r ON r.id = e.rule_id
         ORDER BY e.fired_at DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&pool)
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}
