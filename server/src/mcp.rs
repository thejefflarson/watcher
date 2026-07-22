//! Read-only MCP server (JEF-471) mounted in-process on the axum app at `/mcp`.
//!
//! Exposes watcher's read API as Model Context Protocol tools over the official
//! streamable-HTTP transport (`rmcp`), so an MCP client (MCP Inspector, Claude
//! Code, …) can search traces/logs/metrics and read services/alerts against the
//! same Postgres the UI queries. Every tool is a thin wrapper over an existing
//! `api::query_*` function — the *same* code the HTTP handlers run — so both
//! surfaces share every limit clamp and default time window. There are **no**
//! write/mutate tools; `/mcp` touches no ingest or reconcile path (ADR 0018).
//!
//! `/mcp` is gated behind `WATCHER_MCP_ENABLED` (default OFF) and only mounted
//! when enabled — it carries no auth of its own yet (JEF-472), so it must not be
//! exposed unauthenticated. It mounts *outside* the browser-cookie edge auth the
//! UI/`/api` sit behind (Cloudflare Access, ADR 0013): an MCP client is not a
//! browser and will get its own auth in JEF-472.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use sqlx::PgPool;

use crate::api;

/// Env flag gating `/mcp`. Default OFF (opt-in) — unlike the self-telemetry
/// opt-outs — because the endpoint is unauthenticated until JEF-472 lands.
const ENABLE_FLAG: &str = "WATCHER_MCP_ENABLED";

/// Whether the MCP endpoint should be mounted. Only an explicit truthy value
/// enables it; anything else (including unset) leaves `/mcp` off.
pub fn enabled() -> bool {
    std::env::var(ENABLE_FLAG)
        .map(|v| matches!(v.as_str(), "1" | "true" | "on"))
        .unwrap_or(false)
}

/// The MCP server: holds the shared `PgPool` and the generated tool router.
#[derive(Clone)]
pub struct WatcherMcp {
    pool: PgPool,
    tool_router: ToolRouter<Self>,
}

// --- Tool argument schemas -------------------------------------------------
// These mirror the useful fields of the `api::*Query` structs. Time bounds are
// accepted as RFC3339 strings (LLM-friendly) and parsed to `DateTime<Utc>`.

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct SearchTracesArgs {
    /// Max traces to return (clamped 1–1000, default 100).
    pub limit: Option<i64>,
    /// Filter to a single service name.
    pub service: Option<String>,
    /// Substring match on the trace's root span (operation) name.
    pub name: Option<String>,
    /// Attribute equality filter `key=value`, matched against any span in the trace.
    pub attr: Option<String>,
    /// Only traces containing at least one error span.
    #[serde(default)]
    pub errors_only: bool,
    /// Only traces at least this many milliseconds long (find slow traces).
    pub min_duration_ms: Option<f64>,
    /// Start of the time window (RFC3339). Defaults to 24h ago when omitted.
    pub from: Option<String>,
    /// End of the time window (RFC3339). Unbounded when omitted.
    pub to: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTraceArgs {
    /// Trace id (hex string) whose spans to fetch, ordered for a waterfall.
    pub trace_id: String,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct QueryLogsArgs {
    /// Max log rows to return (clamped 1–2000, default 200).
    pub limit: Option<i64>,
    /// Filter to a single service name.
    pub service: Option<String>,
    /// Filter to one trace's logs.
    pub trace_id: Option<String>,
    /// Filter to one span's logs.
    pub span_id: Option<String>,
    /// Case-insensitive substring match on the log body.
    pub q: Option<String>,
    /// Start of the time window (RFC3339).
    pub from: Option<String>,
    /// End of the time window (RFC3339).
    pub to: Option<String>,
    /// Attribute equality filter `key=value` (e.g. `k8s.pod.name=api-7f`).
    pub attr: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ServicesArgs {
    /// Start of the RED window (RFC3339). Defaults to 24h ago when omitted.
    pub from: Option<String>,
    /// End of the RED window (RFC3339).
    pub to: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct QueryMetricsArgs {
    /// Max metric series to return (clamped 1–2000, default 200).
    pub limit: Option<i64>,
    /// Filter to a single service name.
    pub service: Option<String>,
    /// Start of the sample window (RFC3339).
    pub from: Option<String>,
    /// End of the sample window (RFC3339).
    pub to: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MetricSeriesArgs {
    /// The metric name to plot.
    pub name: String,
    /// Filter to a single service name.
    pub service: Option<String>,
    /// Lookback window in hours (clamped 1–2160, default 24).
    pub hours: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct AlertEventsArgs {
    /// Max transitions to return (clamped 1–1000, default 100).
    pub limit: Option<i64>,
}

// --- Helpers ---------------------------------------------------------------

/// Serialize a typed result as the tool's JSON content. All response shapes are
/// the exact `api` structs, so the JSON matches the `/api` responses.
fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

/// A DB failure is an infrastructure (protocol) error, not a tool-level one.
fn db_error(e: sqlx::Error) -> McpError {
    McpError::internal_error(format!("query failed: {e}"), None)
}

/// Parse an optional RFC3339 time bound, mapping a bad value to invalid_params.
fn parse_time(field: &str, v: Option<String>) -> Result<Option<DateTime<Utc>>, McpError> {
    match v {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(&s)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|e| {
                McpError::invalid_params(
                    format!("`{field}` must be an RFC3339 timestamp: {e}"),
                    None,
                )
            }),
    }
}

#[tool_router]
impl WatcherMcp {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Search recent distributed traces (one row per trace) with \
        optional service, root-operation-name, attribute, error-only, min-duration and \
        time-window filters. Defaults to the last 24 hours."
    )]
    async fn search_traces(
        &self,
        Parameters(a): Parameters<SearchTracesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let q = api::TraceQuery {
            limit: a.limit,
            service: a.service,
            name: a.name,
            attr: a.attr,
            errors_only: a.errors_only,
            min_duration_ms: a.min_duration_ms,
            from: parse_time("from", a.from)?,
            to: parse_time("to", a.to)?,
        };
        let rows = api::query_traces(&self.pool, q).await.map_err(db_error)?;
        json_result(&rows)
    }

    #[tool(
        description = "Fetch every span of a single trace by trace id, ordered by \
        start time for waterfall rendering."
    )]
    async fn get_trace(
        &self,
        Parameters(a): Parameters<GetTraceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let rows = api::query_trace_spans(&self.pool, a.trace_id)
            .await
            .map_err(db_error)?;
        json_result(&rows)
    }

    #[tool(
        description = "Query recent logs with optional service, trace id, span id, \
        body substring, attribute and time-window filters."
    )]
    async fn query_logs(
        &self,
        Parameters(a): Parameters<QueryLogsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let q = api::LogQuery {
            limit: a.limit,
            service: a.service,
            trace_id: a.trace_id,
            span_id: a.span_id,
            q: a.q,
            from: parse_time("from", a.from)?,
            to: parse_time("to", a.to)?,
            attr: a.attr,
        };
        let rows = api::query_logs(&self.pool, q).await.map_err(db_error)?;
        json_result(&rows)
    }

    #[tool(
        description = "List per-service RED metrics (request count, error count + \
        rate, latency p50/p95/p99) over a time window. Defaults to the last 24 hours."
    )]
    async fn list_services(
        &self,
        Parameters(a): Parameters<ServicesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let q = api::RedQuery {
            from: parse_time("from", a.from)?,
            to: parse_time("to", a.to)?,
        };
        let rows = api::query_service_red(&self.pool, q)
            .await
            .map_err(db_error)?;
        json_result(&rows)
    }

    #[tool(
        description = "Return the service dependency graph (nodes + call-count \
        edges) derived from span parent/child links over the last hour."
    )]
    async fn service_map(&self) -> Result<CallToolResult, McpError> {
        let map = api::query_service_map(&self.pool).await.map_err(db_error)?;
        json_result(&map)
    }

    #[tool(
        description = "List metric series with their latest value and a short \
        sparkline, with optional service and time-window filters."
    )]
    async fn query_metrics(
        &self,
        Parameters(a): Parameters<QueryMetricsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let q = api::MetricQuery {
            limit: a.limit,
            service: a.service,
            from: parse_time("from", a.from)?,
            to: parse_time("to", a.to)?,
        };
        let rows = api::query_metrics(&self.pool, q).await.map_err(db_error)?;
        json_result(&rows)
    }

    #[tool(
        description = "Return a time series (bucket-averaged) for one metric name \
        over a lookback window in hours (default 24), optionally filtered by service."
    )]
    async fn metric_series(
        &self,
        Parameters(a): Parameters<MetricSeriesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let q = api::SeriesQuery {
            name: a.name,
            service: a.service,
            hours: a.hours,
        };
        let rows = api::query_metric_series(&self.pool, q)
            .await
            .map_err(db_error)?;
        json_result(&rows)
    }

    #[tool(
        description = "List all configured alert rules with their current firing \
        state and the watched metric's kind/unit."
    )]
    async fn list_alerts(&self) -> Result<CallToolResult, McpError> {
        let rows = api::query_alerts(&self.pool).await.map_err(db_error)?;
        json_result(&rows)
    }

    #[tool(description = "List recent alert firing/resolved transitions, newest first.")]
    async fn alert_events(
        &self,
        Parameters(a): Parameters<AlertEventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let q = api::EventQuery { limit: a.limit };
        let rows = api::query_alert_events(&self.pool, q)
            .await
            .map_err(db_error)?;
        json_result(&rows)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WatcherMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "watcher: a Postgres-native OpenTelemetry traces/logs/metrics backend. \
             All tools are READ-ONLY queries over the same data the UI shows.",
        )
    }
}

/// Build the streamable-HTTP MCP service to nest at `/mcp`. A fresh
/// [`WatcherMcp`] is created per session, each sharing the same `PgPool`.
pub fn service(pool: PgPool) -> StreamableHttpService<WatcherMcp, LocalSessionManager> {
    // The transport defaults to a loopback-only Host allow-list (DNS-rebinding
    // protection for locally-run servers reached by a browser). watcher's MCP is
    // a server-to-server endpoint reached through a public tunnel host and gated
    // by the enable flag (and, per JEF-472, its own auth), so that default would
    // reject every legitimate client. Disable the Host allow-list here; Origin
    // validation stays off since MCP clients are not browsers.
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    StreamableHttpService::new(
        move || Ok(WatcherMcp::new(pool.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}
