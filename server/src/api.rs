//! Query API consumed by the UI. Runtime-checked sqlx queries (no DB needed at build time).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::Instrument;

type ApiError = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// GET /healthz — deep readiness probe: 200 only when the DB is reachable AND
/// retention isn't stalled past the configured age; otherwise 503. This gates
/// *readiness* (traffic), not liveness — a stalled retention or a DB outage
/// should stop new traffic and page, not kill the process (JEF-425).
pub async fn healthz(State(pool): State<PgPool>) -> impl IntoResponse {
    let h = crate::selfmon::health(&pool).await;
    let status = if h.healthy() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = Json(serde_json::json!({
        "status": if h.healthy() { "ok" } else { "unhealthy" },
        "db": h.db_ok,
        "retention_stalled": h.retention_stalled,
        "retention_last_success_age_secs": h.retention_last_success_age_secs,
    }));
    (status, body)
}

#[derive(Deserialize)]
pub struct TraceQuery {
    limit: Option<i64>,
    service: Option<String>,
    /// Substring match on the trace's root span name (operation).
    name: Option<String>,
    /// Attribute equality filter, `key=value`, matched against any span in the trace.
    attr: Option<String>,
    /// Only traces that contain at least one error span.
    #[serde(default)]
    errors_only: bool,
    /// Only traces at least this long (ms) — for finding slow traces.
    min_duration_ms: Option<f64>,
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
#[tracing::instrument(skip_all)]
pub async fn list_traces(
    State(pool): State<PgPool>,
    Query(q): Query<TraceQuery>,
) -> Result<Json<Vec<TraceSummary>>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    // `key=value` → JSONB containment, matched against any span in the trace.
    let attr_json = q
        .attr
        .as_deref()
        .and_then(|s| s.split_once('='))
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| serde_json::json!({ k: v }));
    // service + time bound the spans scanned (index-friendly). The attribute
    // filter selects whole traces that contain a matching span via a subquery the
    // spans_attrs_gin index can serve (a HAVING bool_or couldn't use the index).
    // The remaining trace-level filters (name / errors / duration) are HAVING, so
    // the per-trace aggregates stay computed over the whole trace.
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
           -- default to a recent window when unbounded so this can't turn into a
           -- full-table GROUP BY (which times out); the UI always passes a range.
           AND start_time >= COALESCE($2::timestamptz, now() - interval '24 hours')
           AND ($3::timestamptz IS NULL OR start_time <= $3)
           AND ($6::jsonb IS NULL
                OR trace_id IN (SELECT trace_id FROM spans WHERE attributes @> $6))
         GROUP BY trace_id
         HAVING ($5::text IS NULL
                 OR (array_agg(name ORDER BY start_time))[1] ILIKE '%' || $5 || '%')
            AND (NOT $7::bool OR count(*) FILTER (WHERE status_code = 2) > 0)
            AND ($8::float8 IS NULL
                 OR extract(epoch FROM (max(end_time) - min(start_time))) * 1000.0 >= $8)
         ORDER BY start_time DESC
         LIMIT $4",
    )
    .bind(q.service)
    .bind(q.from)
    .bind(q.to)
    .bind(limit)
    .bind(q.name)
    .bind(attr_json)
    .bind(q.errors_only)
    .bind(q.min_duration_ms)
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
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
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct LogQuery {
    limit: Option<i64>,
    service: Option<String>,
    trace_id: Option<String>,
    /// Narrow to a single span's logs (used by the trace waterfall drill-down).
    span_id: Option<String>,
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
#[tracing::instrument(skip_all)]
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
           AND ($3::text IS NULL OR span_id = $3)
           AND ($4::text IS NULL OR body ILIKE '%' || $4 || '%')
           AND ($5::timestamptz IS NULL OR time >= $5)
           AND ($6::timestamptz IS NULL OR time <= $6)
           AND ($7::jsonb IS NULL OR attributes @> $7)
         ORDER BY time DESC
         LIMIT $8",
    )
    .bind(q.service)
    .bind(q.trace_id)
    .bind(q.span_id)
    .bind(q.q)
    .bind(q.from)
    .bind(q.to)
    .bind(attr_json)
    .bind(limit)
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
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
    /// Up to 30 most-recent raw values (newest first) for an inline sparkline.
    spark: Option<Vec<f64>>,
    /// Histogram observation counts aligned 1:1 with `spark` (so the UI can turn
    /// a histogram's sum into an average-latency glyph: Δsum/Δcount). Null/zeros
    /// for non-histograms.
    count_spark: Option<Vec<f64>>,
    /// Histograms only: the most recent per-bucket counts, so the list can draw a
    /// mini distribution (the histogram's actual shape) rather than a line.
    dist: Option<Vec<i64>>,
    /// Distinct series seen in the recent sample. 1 for a plain metric; >1 flags
    /// a multi-series metric whose per-label breakdown lives in the chart.
    series_count: Option<i64>,
}

/// GET /api/metrics — one row per metric series with its latest value.
#[tracing::instrument(skip_all)]
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
                r.spark, r.count_spark, r.series_count,
                CASE WHEN r.kind = 'histogram' THEN
                    (SELECT m.bucket_counts FROM metrics m
                     WHERE m.name = n.name AND m.bucket_counts IS NOT NULL
                     ORDER BY m.time DESC LIMIT 1)
                END AS dist
         FROM names n
         CROSS JOIN LATERAL (
             -- Read only the 30 most-recent points for the name (index-fast via
             -- metrics_name_time_idx) for the latest value + sparkline, plus a
             -- cheap count of distinct series in that sample so the list can flag
             -- multi-series metrics (×N) and point at the chart's per-label
             -- breakdown. A truly coherent per-series-summed spark needs
             -- per-series rollups (see metric_series_rollups) — too costly to compute
             -- per-request here, where this endpoint is polled.
             SELECT (array_agg(service ORDER BY time DESC))[1] AS service,
                    (array_agg(kind    ORDER BY time DESC))[1] AS kind,
                    (array_agg(unit    ORDER BY time DESC))[1] AS unit,
                    (array_agg(value   ORDER BY time DESC))[1] AS last_value,
                    max(time)                                  AS last_time,
                    array_agg(value ORDER BY time DESC) FILTER (WHERE value IS NOT NULL) AS spark,
                    array_agg(count::float8 ORDER BY time DESC) FILTER (WHERE value IS NOT NULL AND count IS NOT NULL) AS count_spark,
                    count(DISTINCT coalesce(attributes::text, '')) AS series_count
             FROM (
                 SELECT service, kind, unit, value, count, time, attributes
                 FROM metrics
                 WHERE name = n.name
                   AND ($1::text IS NULL OR service = $1)
                   AND ($2::timestamptz IS NULL OR time >= $2)
                   AND ($3::timestamptz IS NULL OR time <= $3)
                 ORDER BY time DESC
                 LIMIT 30
             ) recent
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
    .instrument(tracing::info_span!("db.query"))
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

/// GET /api/metrics/series — a time series for one metric, stitched from the
/// per-series rollups, collapsed across series into one bucket-average value per
/// bucket. Rollups are maintained on ingest, so the current bucket is already
/// present (no raw stitch needed) and the series stays intact after raw is pruned.
pub async fn metric_series(
    State(pool): State<PgPool>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<Vec<SeriesPoint>>, ApiError> {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 90);
    let rows = sqlx::query_as::<_, SeriesPoint>(
        "SELECT bucket AS t, sum(sum) / nullif(sum(count), 0) AS v
         FROM metric_series_rollups
         WHERE name = $1 AND ($2::text IS NULL OR service = $2)
           AND bucket >= now() - make_interval(hours => $3)
         GROUP BY bucket
         ORDER BY t ASC",
    )
    .bind(q.name)
    .bind(q.service)
    .bind(hours)
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
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
    .instrument(tracing::info_span!("db.query"))
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
                metric_bucket(time, $4) AS t,
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
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

// --- Faceted series (one line per full attribute set) ----------------------

#[derive(Deserialize)]
pub struct FacetQuery {
    name: String,
    hours: Option<i32>,
}

#[derive(Serialize)]
pub struct FacetSeries {
    /// The series' full attribute set; the UI labels each line by the keys that
    /// vary across the set (e.g. pod=a, cpu=0).
    attrs: serde_json::Value,
    points: Vec<SeriesPoint>,
}

#[derive(Serialize)]
pub struct FacetResponse {
    kind: Option<String>,
    /// true when values are per-second rates (a monotonic sum / counter).
    rated: bool,
    unit: Option<String>,
    series: Vec<FacetSeries>,
    /// Series omitted beyond the display cap (0 if none).
    truncated: i64,
}

// Generous safety ceiling only — normal metrics return every series. Past this a
// browser would choke on the sparkline count, so we report the remainder.
const FACET_MAX_SERIES: usize = 500;

#[derive(sqlx::FromRow)]
struct FacetRow {
    attrs: serde_json::Value,
    t: DateTime<Utc>,
    avg: Option<f64>,
    last: Option<f64>,
}

/// GET /api/metrics/facet — one bucketed series per distinct attribute set, so a
/// per-pod/per-cpu metric shows each real series instead of one blended line.
/// Gauges plot the bucket average; monotonic sums (counters) are differenced
/// into a per-second rate. Top series by activity; the rest are reported as a
/// count, never silently dropped.
#[tracing::instrument(skip_all)]
pub async fn metric_facet(
    State(pool): State<PgPool>,
    Query(q): Query<FacetQuery>,
) -> Result<Json<FacetResponse>, ApiError> {
    let hours = q.hours.unwrap_or(6).clamp(1, 24 * 7);

    // kind / monotonicity / unit are constant per metric name; read the most
    // recent from raw or rollup (raw may be pruned past metrics_raw_days).
    let meta: Option<(String, Option<bool>, Option<String>)> = sqlx::query_as(
        "SELECT kind, is_monotonic, unit FROM (
             (SELECT kind, is_monotonic, unit, time AS t FROM metrics
              WHERE name = $1 ORDER BY time DESC LIMIT 1)
             UNION ALL
             (SELECT kind, is_monotonic, unit, bucket AS t FROM metric_series_rollups
              WHERE name = $1 ORDER BY bucket DESC LIMIT 1)
         ) z ORDER BY t DESC LIMIT 1",
    )
    .bind(&q.name)
    .fetch_optional(&pool)
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;
    let (kind, is_monotonic, unit) = match meta {
        Some((k, m, u)) => (Some(k), m.unwrap_or(false), u),
        None => {
            return Ok(Json(FacetResponse {
                kind: None,
                rated: false,
                unit: None,
                series: vec![],
                truncated: 0,
            }))
        }
    };
    let rated = kind.as_deref() == Some("sum") && is_monotonic;

    // Per-series points straight from the downsampled rollup — index-fast, no
    // raw scan. Rollups are maintained on ingest, so the current (still-filling)
    // bucket is present, just partial. avg = bucket mean (gauges); max =
    // cumulative level (counters, for rate differencing).
    let rows: Vec<FacetRow> = sqlx::query_as(
        "SELECT attrs, bucket AS t, avg, max AS last
         FROM metric_series_rollups
         WHERE name = $1 AND bucket >= now() - make_interval(hours => $2)
         ORDER BY t ASC",
    )
    .bind(&q.name)
    .bind(hours)
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;

    // Group rows into series keyed by their attribute set.
    let mut map: std::collections::BTreeMap<String, (serde_json::Value, Vec<FacetRow>)> =
        std::collections::BTreeMap::new();
    for r in rows {
        let key = r.attrs.to_string();
        map.entry(key)
            .or_insert_with(|| (r.attrs.clone(), Vec::new()))
            .1
            .push(r);
    }

    let mut series: Vec<FacetSeries> = map
        .into_values()
        .map(|(attrs, pts)| {
            let points = if rated {
                // Difference the cumulative level into a per-second rate; a reset
                // (level drops) yields 0 rather than a spurious negative spike.
                let mut out = Vec::with_capacity(pts.len());
                let mut prev: Option<(DateTime<Utc>, f64)> = None;
                for r in pts {
                    let v = match (prev, r.last) {
                        (Some((pt, pv)), Some(cur)) => {
                            let dt = (r.t - pt).num_seconds() as f64;
                            if dt > 0.0 && cur >= pv {
                                Some((cur - pv) / dt)
                            } else {
                                Some(0.0)
                            }
                        }
                        _ => None,
                    };
                    if let Some(c) = r.last {
                        prev = Some((r.t, c));
                    }
                    out.push(SeriesPoint { t: r.t, v });
                }
                out
            } else {
                pts.into_iter()
                    .map(|r| SeriesPoint { t: r.t, v: r.avg })
                    .collect()
            };
            FacetSeries { attrs, points }
        })
        .collect();

    // Most-active series first; cap and report the remainder.
    series.sort_by_key(|b| std::cmp::Reverse(b.points.len()));
    let truncated = series.len().saturating_sub(FACET_MAX_SERIES) as i64;
    series.truncate(FACET_MAX_SERIES);

    Ok(Json(FacetResponse {
        kind,
        rated,
        unit,
        series,
        truncated,
    }))
}

// --- Histogram percentiles + heatmap ---------------------------------------

#[derive(Deserialize)]
pub struct HistQuery {
    name: String,
    hours: Option<i32>,
}

#[derive(Serialize)]
pub struct HistBucket {
    t: DateTime<Utc>,
    /// Per-value-bucket observation counts (heatmap row), aligned to `bounds`.
    counts: Vec<i64>,
    p50: Option<f64>,
    p95: Option<f64>,
    p99: Option<f64>,
}

#[derive(Serialize)]
pub struct HistResponse {
    /// Explicit upper bounds shared by the buckets (length = counts - 1).
    bounds: Vec<f64>,
    unit: Option<String>,
    buckets: Vec<HistBucket>,
}

#[derive(sqlx::FromRow)]
struct HistRow {
    t: DateTime<Utc>,
    bounds: Option<Vec<f64>>,
    counts: Option<Vec<i64>>,
    unit: Option<String>,
}

/// GET /api/metrics/histogram — per-time-bucket distribution for a histogram
/// metric: the summed bucket counts (a heatmap row) plus p50/p95/p99 computed by
/// linear interpolation across the buckets. Counts are summed within each time
/// bucket (delta temporality — each data point carries its interval's counts).
#[tracing::instrument(skip_all)]
pub async fn metric_histogram(
    State(pool): State<PgPool>,
    Query(q): Query<HistQuery>,
) -> Result<Json<HistResponse>, ApiError> {
    let hours = q.hours.unwrap_or(6).clamp(1, 24 * 7);

    // Per-series rollup counts (already summed within series+bucket) straight
    // from the rollup; the Rust pass below sums across series per time bucket.
    let rows: Vec<HistRow> = sqlx::query_as(
        "SELECT bucket AS t, bucket_bounds AS bounds, bucket_counts AS counts, unit
         FROM metric_series_rollups
         WHERE name = $1 AND kind = 'histogram' AND bucket_counts IS NOT NULL
           AND bucket >= now() - make_interval(hours => $2)
         ORDER BY t ASC",
    )
    .bind(&q.name)
    .bind(hours)
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;

    // Element-wise sum the counts within each time bucket. Use the first row's
    // bounds as the reference and skip points with a mismatched schema.
    let mut bounds: Vec<f64> = Vec::new();
    let mut unit: Option<String> = None;
    let mut agg: std::collections::BTreeMap<i64, (DateTime<Utc>, Vec<i64>)> =
        std::collections::BTreeMap::new();
    for r in rows {
        let (b, c) = match (r.bounds, r.counts) {
            (Some(b), Some(c)) if c.len() == b.len() + 1 => (b, c),
            _ => continue,
        };
        if bounds.is_empty() {
            bounds = b;
            unit = r.unit;
        }
        if c.len() != bounds.len() + 1 {
            continue;
        }
        let entry = agg
            .entry(r.t.timestamp())
            .or_insert_with(|| (r.t, vec![0i64; c.len()]));
        for (s, v) in entry.1.iter_mut().zip(c.iter()) {
            *s += *v;
        }
    }

    let buckets = agg
        .into_values()
        .map(|(t, counts)| HistBucket {
            p50: hist_quantile(&bounds, &counts, 0.50),
            p95: hist_quantile(&bounds, &counts, 0.95),
            p99: hist_quantile(&bounds, &counts, 0.99),
            t,
            counts,
        })
        .collect();

    Ok(Json(HistResponse {
        bounds,
        unit,
        buckets,
    }))
}

/// Linear-interpolated quantile from explicit-bound histogram buckets, à la
/// Prometheus `histogram_quantile`. `bounds` are the upper bounds; `counts` has
/// one extra entry for the +Inf overflow bucket.
fn hist_quantile(bounds: &[f64], counts: &[i64], q: f64) -> Option<f64> {
    let total: i64 = counts.iter().sum();
    if total == 0 || bounds.is_empty() {
        return None;
    }
    let rank = q * total as f64;
    let mut cum = 0i64;
    for (i, &c) in counts.iter().enumerate() {
        let prev_cum = cum;
        cum += c;
        if cum as f64 >= rank {
            let lower = if i == 0 { 0.0 } else { bounds[i - 1] };
            // +Inf overflow bucket: clamp to the largest finite bound.
            let upper = match bounds.get(i) {
                Some(&u) => u,
                None => return bounds.last().copied(),
            };
            if c == 0 {
                return Some(upper);
            }
            let frac = (rank - prev_cum as f64) / c as f64;
            return Some(lower + (upper - lower) * frac);
        }
    }
    bounds.last().copied()
}

// --- Per-series histogram percentiles (for the expandable list) ------------

#[derive(Serialize)]
pub struct HistFacetPoint {
    t: DateTime<Utc>,
    p50: Option<f64>,
    p95: Option<f64>,
    p99: Option<f64>,
}

#[derive(Serialize)]
pub struct HistFacetSeries {
    attrs: serde_json::Value,
    points: Vec<HistFacetPoint>,
    /// This series' most recent per-bucket counts, so the row can draw the
    /// distribution shape (bars) rather than just a percentile line.
    dist: Vec<i64>,
}

#[derive(Serialize)]
pub struct HistFacetResponse {
    unit: Option<String>,
    /// Bucket upper bounds shared by the series' `dist` arrays.
    bounds: Vec<f64>,
    series: Vec<HistFacetSeries>,
    truncated: i64,
}

#[derive(sqlx::FromRow)]
struct HistFacetRow {
    attrs: serde_json::Value,
    t: DateTime<Utc>,
    bounds: Option<Vec<f64>>,
    counts: Option<Vec<i64>>,
    unit: Option<String>,
}

/// GET /api/metrics/hist_facet — per-series p50/p95/p99 over time for a histogram
/// metric, so the expandable list can show each series' latest percentiles and a
/// p95 trend. Rollup-backed (+ recent raw), interpolated with `hist_quantile`.
#[tracing::instrument(skip_all)]
pub async fn metric_hist_facet(
    State(pool): State<PgPool>,
    Query(q): Query<HistQuery>,
) -> Result<Json<HistFacetResponse>, ApiError> {
    let hours = q.hours.unwrap_or(6).clamp(1, 24 * 7);

    let rows: Vec<HistFacetRow> = sqlx::query_as(
        "SELECT attrs, bucket AS t, bucket_bounds AS bounds, bucket_counts AS counts, unit
         FROM metric_series_rollups
         WHERE name = $1 AND kind = 'histogram' AND bucket_counts IS NOT NULL
           AND bucket >= now() - make_interval(hours => $2)
         ORDER BY t ASC",
    )
    .bind(&q.name)
    .bind(hours)
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;

    let mut unit: Option<String> = None;
    let mut bounds: Vec<f64> = Vec::new();
    // Rows are ordered t ASC, so the last counts seen per series is the latest
    // distribution.
    #[allow(clippy::type_complexity)]
    let mut map: std::collections::BTreeMap<
        String,
        (serde_json::Value, Vec<HistFacetPoint>, Vec<i64>),
    > = std::collections::BTreeMap::new();
    for r in rows {
        if unit.is_none() {
            unit = r.unit;
        }
        let (b, c) = match (r.bounds, r.counts) {
            (Some(b), Some(c)) if c.len() == b.len() + 1 => (b, c),
            _ => continue,
        };
        if bounds.is_empty() {
            bounds = b.clone();
        }
        let point = HistFacetPoint {
            t: r.t,
            p50: hist_quantile(&b, &c, 0.50),
            p95: hist_quantile(&b, &c, 0.95),
            p99: hist_quantile(&b, &c, 0.99),
        };
        let entry = map
            .entry(r.attrs.to_string())
            .or_insert_with(|| (r.attrs, Vec::new(), Vec::new()));
        entry.1.push(point);
        entry.2 = c;
    }

    let mut series: Vec<HistFacetSeries> = map
        .into_values()
        .map(|(attrs, points, dist)| HistFacetSeries {
            attrs,
            points,
            dist,
        })
        .collect();
    series.sort_by_key(|b| std::cmp::Reverse(b.points.len()));
    let truncated = series.len().saturating_sub(FACET_MAX_SERIES) as i64;
    series.truncate(FACET_MAX_SERIES);

    Ok(Json(HistFacetResponse {
        unit,
        bounds,
        series,
        truncated,
    }))
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
#[tracing::instrument(skip_all)]
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
           -- default to a recent window when unbounded so the per-service
           -- percentile aggregate can't full-scan the spans table and time out.
           AND start_time >= COALESCE($1::timestamptz, now() - interval '24 hours')
           AND ($2::timestamptz IS NULL OR start_time <= $2)
         GROUP BY service
         ORDER BY spans DESC",
    )
    .bind(q.from)
    .bind(q.to)
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
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
#[tracing::instrument(skip_all)]
pub async fn service_map(State(pool): State<PgPool>) -> Result<Json<ServiceMap>, ApiError> {
    // Current topology only: bound to the recent window so this is a tiny
    // index scan, not a self-join over the whole (retention-deep) spans table.
    let nodes: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT service FROM spans
         WHERE service IS NOT NULL AND start_time > now() - interval '1 hour'
         ORDER BY 1",
    )
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;

    let edges = sqlx::query_as::<_, ServiceEdge>(
        "SELECT parent.service AS source, child.service AS target, count(*) AS calls
         FROM spans child
         JOIN spans parent
           ON child.parent_span_id = parent.span_id
          AND child.trace_id = parent.trace_id
         WHERE child.start_time > now() - interval '1 hour'
           AND parent.service IS NOT NULL
           AND child.service IS NOT NULL
           AND parent.service IS DISTINCT FROM child.service
         GROUP BY parent.service, child.service
         ORDER BY calls DESC",
    )
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;

    Ok(Json(ServiceMap { nodes, edges }))
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

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
    /// The watched metric's kind (gauge | sum | histogram) and unit, joined so the
    /// UI can deep-link to the right chart type — a histogram alert must open the
    /// histogram view, not a line chart. Null if the metric has no points yet.
    kind: Option<String>,
    unit: Option<String>,
}

// The `m.kind, m.unit` columns come from the LATERAL join below: the most-recent
// kind/unit for the rule's metric (both constant per metric name), read from raw or
// rollup since raw may be pruned past retention. The UI uses them to deep-link to
// the correct chart type. Correlated on the `r`-aliased alert_rules row.

/// GET /api/alerts — all rules with their current firing state.
pub async fn list_alerts(State(pool): State<PgPool>) -> Result<Json<Vec<AlertRuleView>>, ApiError> {
    let rows = sqlx::query_as::<_, AlertRuleView>(
        "SELECT r.id, r.name, r.metric, r.service, r.comparator, r.threshold, r.agg,
                r.window_secs, r.enabled, r.created_at,
                (e.id IS NOT NULL) AS firing, m.kind, m.unit
         FROM alert_rules r
         -- Only an *activated* open event counts as firing: a pending event (a
         -- breach still dwelling toward its `for` window) has active_at NULL and
         -- must not surface as firing until it matures. See alerts.rs / ADR 0015.
         LEFT JOIN alert_events e
                ON e.rule_id = r.id AND e.resolved_at IS NULL AND e.active_at IS NOT NULL
         LEFT JOIN LATERAL (
             SELECT kind, unit FROM (
                 (SELECT kind, unit, time AS t FROM metrics
                  WHERE name = r.metric ORDER BY time DESC LIMIT 1)
                 UNION ALL
                 (SELECT kind, unit, bucket AS t FROM metric_series_rollups
                  WHERE name = r.metric ORDER BY bucket DESC LIMIT 1)
             ) z ORDER BY t DESC LIMIT 1
         ) m ON true
         ORDER BY r.created_at DESC",
    )
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}

// Rules are declarative (reconciled from config on startup — see alerts.rs), so
// there is no create/delete API; `/api/alerts` is read-only.

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
    /// Watched metric's kind/unit — same join as the rules view, so a deep-link
    /// from an event history strip lands on the correct chart type.
    kind: Option<String>,
    unit: Option<String>,
}

/// GET /api/alerts/events — recent firing/resolved transitions, newest first.
pub async fn list_alert_events(
    State(pool): State<PgPool>,
    Query(q): Query<EventQuery>,
) -> Result<Json<Vec<AlertEventView>>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let rows = sqlx::query_as::<_, AlertEventView>(
        // Same metric-metadata LATERAL join as list_alerts (see note there).
        "SELECT e.id, e.rule_id, r.name AS rule_name, r.metric, e.value, e.fired_at,
                e.resolved_at, m.kind, m.unit
         FROM alert_events e
         JOIN alert_rules r ON r.id = e.rule_id
         LEFT JOIN LATERAL (
             SELECT kind, unit FROM (
                 (SELECT kind, unit, time AS t FROM metrics
                  WHERE name = r.metric ORDER BY time DESC LIMIT 1)
                 UNION ALL
                 (SELECT kind, unit, bucket AS t FROM metric_series_rollups
                  WHERE name = r.metric ORDER BY bucket DESC LIMIT 1)
             ) z ORDER BY t DESC LIMIT 1
         ) m ON true
         -- A pending (not-yet-activated) event is not a transition; only surface
         -- events that actually fired so the feed stays an honest firing/resolved log.
         WHERE e.active_at IS NOT NULL
         ORDER BY e.fired_at DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&pool)
    .instrument(tracing::info_span!("db.query"))
    .await
    .map_err(internal)?;
    Ok(Json(rows))
}
