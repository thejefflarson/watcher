//! Alerting: evaluate threshold rules against recent metric points on a timer.
//! Each breach opens an `alert_events` row (resolved when the value recovers),
//! logs the transition, and optionally POSTs a webhook. Storage + log are always
//! on; the webhook fires only when `WATCHER_ALERT_WEBHOOK` is set.
//!
//! Rules are **declarative**: a JSON config file (rendered from the chart's
//! values) is the source of truth. [`reconcile`] applies it into `alert_rules`
//! on startup — upsert by name, delete what's no longer declared — so the table
//! is read-only at the API layer.

use anyhow::Context as _;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::time::Duration;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// A rule as stored. Shared by the evaluator and the read API.
#[derive(sqlx::FromRow)]
pub struct AlertRule {
    pub id: i64,
    pub name: String,
    pub metric: String,
    pub service: Option<String>,
    pub comparator: String,
    pub threshold: f64,
    pub agg: String,
    pub window_secs: i32,
    /// JSONB the series MUST contain (`attributes @> match_attrs`); NULL = no filter.
    pub match_attrs: Option<serde_json::Value>,
    /// JSONB the series must NOT contain (`NOT attributes @> exclude_attrs`); NULL = no filter.
    pub exclude_attrs: Option<serde_json::Value>,
    /// Evaluate a per-second rate rather than the raw level. NULL = auto (on for
    /// monotonic sums), resolved at eval time; Some(_) = explicit override.
    pub rate: Option<bool>,
    /// Require the breach to hold continuously for this many seconds before the
    /// rule fires (`for: 5m`). NULL/0 = fire on the first breach.
    pub for_secs: Option<i32>,
}

fn default_agg() -> String {
    "avg".to_string()
}
fn default_window() -> i32 {
    300
}
fn default_enabled() -> bool {
    true
}

/// One declared rule, as it appears in the JSON config file. Field names match
/// the stored/wire shape 1:1 so the chart's values map straight through.
#[derive(Deserialize)]
pub struct RuleConfig {
    pub name: String,
    pub metric: String,
    #[serde(default)]
    pub service: Option<String>,
    pub comparator: String,
    pub threshold: f64,
    #[serde(default = "default_agg")]
    pub agg: String,
    #[serde(default = "default_window")]
    pub window_secs: i32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Attribute key=value pairs the series MUST have. Empty/absent = no filter.
    #[serde(default, rename = "match")]
    pub match_attrs: Option<serde_json::Map<String, serde_json::Value>>,
    /// Attribute key=value pairs the series must NOT have. Empty/absent = no filter.
    #[serde(default)]
    pub exclude: Option<serde_json::Map<String, serde_json::Value>>,
    /// Difference a cumulative counter into a per-second rate before aggregating.
    /// Absent = auto (on for monotonic sums); explicit true/false overrides.
    #[serde(default)]
    pub rate: Option<bool>,
    /// Require the condition to hold continuously for this many seconds before the
    /// rule fires (`for: 5m`). Absent/0 = fire on the first breach.
    #[serde(default)]
    pub for_secs: Option<i32>,
}

/// Upper bound on `for_secs`. A rule's dwell window must sit well under the raw-
/// metric retention floor (6h default, ADR 0007) so a full window of points is
/// still queryable when the rule matures; 3h is half that, comfortably clear.
const MAX_FOR_SECS: i32 = 10_800;

/// A non-empty JSONB attribute predicate, or None. Mirrors the empty-string
/// handling for `service`: an empty object means "no filter", not "match nothing",
/// so it's normalized to NULL rather than written as `{}` (which `@>` treats as
/// always-true and would make an `exclude` suppress every series).
fn attr_predicate(
    map: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<serde_json::Value> {
    map.as_ref()
        .filter(|m| !m.is_empty())
        .map(|m| serde_json::Value::Object(m.clone()))
}

/// Read declared rules from a JSON file (a list of [`RuleConfig`]). Missing or
/// malformed config is an error so a bad edit fails loudly rather than silently
/// dropping rules.
pub fn load_rules(path: &str) -> anyhow::Result<Vec<RuleConfig>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading alerts config {path}"))?;
    serde_json::from_str(&text).with_context(|| format!("parsing alerts config {path}"))
}

/// Reject a rule whose enum-ish fields fall outside the whitelist the evaluator's
/// SQL relies on (same checks the old create API ran), so reconcile never writes
/// a value `agg_expr`/`breached` can't handle.
fn validate(r: &RuleConfig) -> anyhow::Result<()> {
    if r.name.trim().is_empty() || r.metric.trim().is_empty() {
        anyhow::bail!("alert rule '{}': name and metric are required", r.name);
    }
    if !matches!(r.comparator.as_str(), "gt" | "lt") {
        anyhow::bail!("alert rule '{}': comparator must be 'gt' or 'lt'", r.name);
    }
    if !matches!(r.agg.as_str(), "avg" | "max" | "min" | "sum" | "last") {
        anyhow::bail!(
            "alert rule '{}': agg must be one of avg|max|min|sum|last",
            r.name
        );
    }
    if let Some(f) = r.for_secs {
        if !(1..=MAX_FOR_SECS).contains(&f) {
            anyhow::bail!(
                "alert rule '{}': for_secs must be between 1 and {MAX_FOR_SECS} \
                 (a dwell window well under raw-metric retention)",
                r.name
            );
        }
    }
    Ok(())
}

/// Apply the declared rules into `alert_rules`: validate all (a bad rule aborts
/// the whole apply, so the table never lands in a partial state), upsert each by
/// name (preserving its id and event history across restarts), then delete any
/// stored rule that's no longer declared. Runs in one transaction.
pub async fn reconcile(pool: &PgPool, rules: &[RuleConfig]) -> anyhow::Result<()> {
    for r in rules {
        validate(r)?;
    }

    let mut tx = pool.begin().await?;
    for r in rules {
        let service = r.service.as_deref().filter(|s| !s.is_empty());
        let window_secs = r.window_secs.clamp(10, 86_400);
        let match_attrs = attr_predicate(&r.match_attrs);
        let exclude_attrs = attr_predicate(&r.exclude);
        sqlx::query(
            "INSERT INTO alert_rules
               (name, metric, service, comparator, threshold, agg, window_secs, enabled,
                match_attrs, exclude_attrs, rate, for_secs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (name) DO UPDATE SET
               metric        = EXCLUDED.metric,
               service       = EXCLUDED.service,
               comparator    = EXCLUDED.comparator,
               threshold     = EXCLUDED.threshold,
               agg           = EXCLUDED.agg,
               window_secs   = EXCLUDED.window_secs,
               enabled       = EXCLUDED.enabled,
               match_attrs   = EXCLUDED.match_attrs,
               exclude_attrs = EXCLUDED.exclude_attrs,
               rate          = EXCLUDED.rate,
               for_secs      = EXCLUDED.for_secs",
        )
        .bind(&r.name)
        .bind(&r.metric)
        .bind(service)
        .bind(&r.comparator)
        .bind(r.threshold)
        .bind(&r.agg)
        .bind(window_secs)
        .bind(r.enabled)
        .bind(&match_attrs)
        .bind(&exclude_attrs)
        .bind(r.rate)
        .bind(r.for_secs)
        .execute(&mut *tx)
        .await?;
    }

    // Drop rules no longer declared (their open/closed events cascade). With an
    // empty config this clears the table — declaring nothing means no alerts.
    let names: Vec<String> = rules.iter().map(|r| r.name.clone()).collect();
    sqlx::query("DELETE FROM alert_rules WHERE name <> ALL($1)")
        .bind(&names)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

#[derive(Serialize)]
struct WebhookPayload<'a> {
    rule: &'a str,
    metric: &'a str,
    service: Option<&'a str>,
    state: &'a str, // "firing" | "resolved"
    value: Option<f64>,
    threshold: f64,
    comparator: &'a str,
}

/// SMTP settings for emailing alert transitions. All fields come from the
/// `WATCHER_ALERT_SMTP_*` env vars; absent/empty config disables email (the
/// webhook and the stored events + log line are independent of this).
pub struct EmailConfig {
    pub relay: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    pub to: String,
}

/// A built mailer: an async STARTTLS SMTP transport plus the parsed from/to
/// addresses. Constructed once in `run`; building is fallible (bad address or
/// relay), in which case email is logged-and-disabled rather than fatal.
struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    to: Mailbox,
}

impl Mailer {
    fn build(cfg: &EmailConfig) -> anyhow::Result<Self> {
        let from: Mailbox = cfg.from.parse()?;
        let to: Mailbox = cfg.to.parse()?;
        let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.relay)?
            .port(cfg.port)
            .credentials(Credentials::new(cfg.username.clone(), cfg.password.clone()))
            // Bound the SMTP handshake/send: a slow relay must give up in seconds so
            // it can't stall the alert-eval loop (`notify` is awaited on the tick).
            .timeout(Some(Duration::from_secs(10)))
            .build();
        Ok(Self {
            transport,
            from,
            to,
        })
    }

    async fn send(&self, subject: &str, body: String) -> anyhow::Result<()> {
        let email = Message::builder()
            .from(self.from.clone())
            .to(self.to.clone())
            .subject(subject)
            .body(body)?;
        self.transport.send(email).await?;
        Ok(())
    }
}

/// Email subject for a transition, e.g. `[watcher] FIRING: pod memory > 80%`.
fn email_subject(rule_name: &str, state: &str) -> String {
    format!("[watcher] {}: {}", state.to_uppercase(), rule_name)
}

/// Human-readable email body describing the rule and the observed value.
fn email_body(rule: &AlertRule, state: &str, value: Option<f64>) -> String {
    let scope = rule
        .service
        .as_deref()
        .map(|s| format!(" (service {s})"))
        .unwrap_or_default();
    let observed = value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "no data".to_string());
    format!(
        "Alert: {name}\n\
         State: {state}\n\
         Metric: {metric}{scope}\n\
         Condition: {agg}({metric}) {cmp} {threshold} over {window}s\n\
         Observed: {observed}\n",
        name = rule.name,
        metric = rule.metric,
        agg = rule.agg,
        cmp = rule.comparator,
        threshold = rule.threshold,
        window = rule.window_secs,
    )
}

/// Whitelisted aggregate expression — `agg` comes from the DB but is only ever
/// written through the validated API, and we map it here rather than interpolate.
fn agg_expr(agg: &str) -> &'static str {
    match agg {
        "max" => "max(value)",
        "min" => "min(value)",
        "sum" => "sum(value)",
        "last" => "(array_agg(value ORDER BY time DESC))[1]",
        _ => "avg(value)",
    }
}

/// Build the value-source-plus-aggregate for one evaluation pass. Parameters are
/// `$1` metric, `$2` service, `$3` window_secs, `$4` match_attrs, `$5` exclude_attrs
/// — every scalar/JSONB operand is bound; only the enum-whitelisted `agg_expr` is
/// interpolated, so the result is injection-safe (asserted at the call site).
///
/// When `rate` is set the cumulative counter level is differenced per series into
/// a per-second rate *before* aggregating, reset-safe like `/api/metrics/facet`:
/// a level that drops (counter reset) yields 0 for that interval, never a spike.
fn eval_sql(agg: &str, rate: bool) -> String {
    // Residual JSONB predicates ride on top of the (name, time) index narrowing;
    // a NULL operand disables its clause so pre-JEF-426 rules are unaffected.
    let filtered = "SELECT time, service, attributes, value
             FROM metrics
             WHERE name = $1
               AND ($2::text IS NULL OR service = $2)
               AND ($4::jsonb IS NULL OR attributes @> $4)
               AND ($5::jsonb IS NULL OR NOT (attributes @> $5))
               AND value IS NOT NULL
               AND time >= now() - make_interval(secs => $3)";
    if rate {
        format!(
            "WITH pts AS ({filtered}),
             rated AS (
                 SELECT time,
                        -- reset (level drops) or non-positive dt → 0, not a spike.
                        CASE WHEN dt > 0 AND dv >= 0 THEN dv / dt ELSE 0 END AS value
                 FROM (
                     SELECT time,
                            value - lag(value) OVER w                     AS dv,
                            extract(epoch FROM time - lag(time) OVER w)   AS dt
                     FROM pts
                     WINDOW w AS (PARTITION BY service, attributes ORDER BY time)
                 ) d
                 WHERE dt IS NOT NULL   -- each series' first point has no predecessor
             )
             SELECT {} FROM rated",
            agg_expr(agg)
        )
    } else {
        format!("WITH pts AS ({filtered}) SELECT {} FROM pts", agg_expr(agg))
    }
}

/// Resolve whether to rate-difference this rule: an explicit `rate` wins; otherwise
/// auto-detect — a monotonic Sum (counter) is rated, everything else is not. Kind
/// and monotonicity are constant per metric name, so the latest stored point decides.
async fn resolve_rate(pool: &PgPool, rule: &AlertRule) -> anyhow::Result<bool> {
    if let Some(explicit) = rule.rate {
        return Ok(explicit);
    }
    let monotonic: Option<bool> = sqlx::query_scalar(
        "SELECT kind = 'sum' AND coalesce(is_monotonic, false)
         FROM metrics
         WHERE name = $1 AND value IS NOT NULL
         ORDER BY time DESC
         LIMIT 1",
    )
    .bind(&rule.metric)
    .fetch_optional(pool)
    .await?;
    Ok(monotonic.unwrap_or(false))
}

/// Whether `value` breaches the rule. No data in the window (`None`) never fires.
fn breached(value: Option<f64>, comparator: &str, threshold: f64) -> bool {
    match (value, comparator) {
        (Some(v), "gt") => v > threshold,
        (Some(v), "lt") => v < threshold,
        _ => false,
    }
}

/// The HTTP client for webhook delivery, bounded so a slow/unresponsive receiver
/// can't stall the eval loop (`notify` is awaited on the tick): 10s total per
/// request, 5s to connect — short enough a dead sink gives up in seconds, long
/// enough for a legitimately slow-but-alive receiver. Shared by `run` and
/// `evaluate_once` so both delivery paths get identical bounds. Fallible (the TLS
/// backend init can fail); callers log and disable the webhook rather than panic.
fn build_webhook_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .build()
}

/// Runs forever; ticks immediately, then every `interval_secs`.
pub async fn run(
    pool: PgPool,
    webhook: Option<String>,
    email: Option<EmailConfig>,
    interval_secs: u64,
) {
    // A failed client build disables the webhook (logged) rather than taking down
    // the alert loop, consistent with how a bad SMTP config disables email below.
    let (client, webhook) = match build_webhook_client() {
        Ok(c) => (c, webhook),
        Err(e) => {
            tracing::error!("alert webhook disabled — failed to build HTTP client: {e}");
            (reqwest::Client::new(), None)
        }
    };
    // Build the mailer once. A bad address/relay disables email (logged) rather
    // than taking down the alert loop.
    let mailer = match email.as_ref().map(Mailer::build) {
        Some(Ok(m)) => Some(m),
        Some(Err(e)) => {
            tracing::error!("alert email disabled — invalid SMTP config: {e}");
            None
        }
        None => None,
    };
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs.max(5)));
    loop {
        interval.tick().await;
        if let Err(e) = evaluate(&pool, webhook.as_deref(), mailer.as_ref(), &client).await {
            tracing::warn!("alert evaluation failed: {e}");
        }
    }
}

/// A single evaluation pass — opens/resolves events for every enabled rule.
/// Exposed for tests and one-shot use; `run` calls it on a timer. Email is only
/// wired through `run` (it owns the built mailer), so this path is webhook-only.
pub async fn evaluate_once(pool: &PgPool, webhook: Option<&str>) -> anyhow::Result<()> {
    // Same bounded client as `run`; a failed build disables the webhook (logged).
    let (client, webhook) = match build_webhook_client() {
        Ok(c) => (c, webhook),
        Err(e) => {
            tracing::error!("alert webhook disabled — failed to build HTTP client: {e}");
            (reqwest::Client::new(), None)
        }
    };
    evaluate(pool, webhook, None, &client).await
}

async fn evaluate(
    pool: &PgPool,
    webhook: Option<&str>,
    mailer: Option<&Mailer>,
    client: &reqwest::Client,
) -> anyhow::Result<()> {
    let rules = sqlx::query_as::<_, AlertRule>(
        "SELECT id, name, metric, service, comparator, threshold, agg, window_secs,
                match_attrs, exclude_attrs, rate, for_secs
         FROM alert_rules WHERE enabled = TRUE",
    )
    .fetch_all(pool)
    .await?;

    for rule in rules {
        let rate = resolve_rate(pool, &rule).await?;
        let sql = eval_sql(&rule.agg, rate);
        // AssertSqlSafe: sqlx 0.9 requires dynamic SQL to be audited. Only the
        // enum-whitelisted agg_expr() is interpolated; metric/service/window and
        // the match/exclude JSONB predicates are all bound, so it's injection-safe.
        let value: Option<f64> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(&rule.metric)
            .bind(&rule.service)
            .bind(rule.window_secs as f64)
            .bind(&rule.match_attrs)
            .bind(&rule.exclude_attrs)
            .fetch_one(pool)
            .await?;

        let breached = breached(value, &rule.comparator, rule.threshold);
        // NULL/0 for_secs = fire on first breach; otherwise the breach must hold
        // continuously for this long (a "pending" event) before the rule pages.
        let for_secs = rule.for_secs.unwrap_or(0).max(0);

        // The single open (unresolved) event for this rule, if any, with whether it
        // has already activated (fired) and whether it has now dwelled past for_secs.
        // Reuses the partial unique index — at most one open row exists per rule.
        let open: Option<(i64, bool, bool)> = sqlx::query_as(
            "SELECT id,
                    active_at IS NOT NULL,
                    now() - fired_at >= make_interval(secs => $2)
             FROM alert_events WHERE rule_id = $1 AND resolved_at IS NULL",
        )
        .bind(rule.id)
        .bind(for_secs as f64)
        .fetch_optional(pool)
        .await?;

        match (breached, open) {
            // First breach: open an event. With no dwell window it activates (fires)
            // immediately; with a `for` window it stays pending until it matures.
            (true, None) => {
                let active = for_secs == 0;
                sqlx::query(
                    "INSERT INTO alert_events (rule_id, value, active_at)
                     VALUES ($1, $2, CASE WHEN $3 THEN now() ELSE NULL END)",
                )
                .bind(rule.id)
                .bind(value)
                .bind(active)
                .execute(pool)
                .await?;
                if active {
                    fire(client, webhook, mailer, &rule, value).await;
                }
            }
            // Still breaching with an open event. A pending event that has now held
            // for the full window activates and fires; an already-firing event is
            // left untouched (no re-notify), and a still-pending one keeps waiting.
            (true, Some((id, active, matured))) => {
                if !active && matured {
                    sqlx::query(
                        "UPDATE alert_events SET active_at = now(), value = $2 WHERE id = $1",
                    )
                    .bind(id)
                    .bind(value)
                    .execute(pool)
                    .await?;
                    fire(client, webhook, mailer, &rule, value).await;
                }
            }
            // Recovered. A firing event resolves and notifies as before; a pending
            // event that never matured is dropped silently (nothing ever fired), so
            // a flap that clears before its window pages no one.
            (false, Some((id, active, _))) => {
                if active {
                    sqlx::query("UPDATE alert_events SET resolved_at = now() WHERE id = $1")
                        .bind(id)
                        .execute(pool)
                        .await?;
                    tracing::info!("alert '{}' resolved (value {:?})", rule.name, value);
                    notify(client, webhook, mailer, &rule, "resolved", value).await;
                } else {
                    sqlx::query("DELETE FROM alert_events WHERE id = $1")
                        .bind(id)
                        .execute(pool)
                        .await?;
                    tracing::debug!(
                        "alert '{}' pending breach cleared before firing (value {:?})",
                        rule.name,
                        value
                    );
                }
            }
            (false, None) => {}
        }
    }
    Ok(())
}

/// Injects W3C trace headers (`traceparent`, …) into the outbound webhook so the
/// receiver can continue watcher's trace.
struct HeaderInjector<'a>(&'a mut reqwest::header::HeaderMap);
impl opentelemetry::propagation::Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}

/// Log and notify a firing transition. Shared by the immediate (no-`for`) and the
/// matured (`for`-window) activation paths so both emit an identical page.
async fn fire(
    client: &reqwest::Client,
    webhook: Option<&str>,
    mailer: Option<&Mailer>,
    rule: &AlertRule,
    value: Option<f64>,
) {
    tracing::warn!(
        "alert '{}' firing: {} {} {} (value {:?})",
        rule.name,
        rule.metric,
        rule.comparator,
        rule.threshold,
        value
    );
    notify(client, webhook, mailer, rule, "firing", value).await;
}

#[tracing::instrument(name = "alert.notify", skip_all, fields(rule = %rule.name, state))]
async fn notify(
    client: &reqwest::Client,
    webhook: Option<&str>,
    mailer: Option<&Mailer>,
    rule: &AlertRule,
    state: &str,
    value: Option<f64>,
) {
    // Webhook and email are independent sinks — either, both, or neither may be
    // configured, and a failure in one must not skip the other.
    if let Some(url) = webhook {
        let payload = WebhookPayload {
            rule: &rule.name,
            metric: &rule.metric,
            service: rule.service.as_deref(),
            state,
            value,
            threshold: rule.threshold,
            comparator: &rule.comparator,
        };
        // Propagate this span's context to the receiver.
        let mut headers = reqwest::header::HeaderMap::new();
        let cx = tracing::Span::current().context();
        opentelemetry::global::get_text_map_propagator(|p| {
            p.inject_context(&cx, &mut HeaderInjector(&mut headers))
        });
        if let Err(e) = client
            .post(url)
            .headers(headers)
            .json(&payload)
            .send()
            .await
        {
            tracing::warn!("alert webhook POST failed: {e}");
        }
    }

    if let Some(mailer) = mailer {
        let subject = email_subject(&rule.name, state);
        let body = email_body(rule, state, value);
        if let Err(e) = mailer.send(&subject, body).await {
            tracing::warn!("alert email send failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        agg_expr, breached, build_webhook_client, email_body, email_subject, eval_sql, notify,
        validate, AlertRule, RuleConfig, MAX_FOR_SECS,
    };

    #[test]
    fn config_parses_with_defaults() {
        // Only the required fields given; the rest fall back to defaults.
        let rules: Vec<RuleConfig> = serde_json::from_str(
            r#"[{"name":"hot","metric":"cpu","comparator":"gt","threshold":1.5}]"#,
        )
        .unwrap();
        assert_eq!(rules.len(), 1);
        let r = &rules[0];
        assert_eq!(r.agg, "avg");
        assert_eq!(r.window_secs, 300);
        assert!(r.enabled);
        assert!(r.service.is_none());
    }

    #[test]
    fn config_honors_explicit_fields() {
        let rules: Vec<RuleConfig> = serde_json::from_str(
            r#"[{"name":"x","metric":"m","service":"api","comparator":"lt",
                 "threshold":0,"agg":"max","window_secs":60,"enabled":false}]"#,
        )
        .unwrap();
        let r = &rules[0];
        assert_eq!(r.service.as_deref(), Some("api"));
        assert_eq!(r.agg, "max");
        assert_eq!(r.window_secs, 60);
        assert!(!r.enabled);
    }

    fn cfg(name: &str, comparator: &str, agg: &str) -> RuleConfig {
        RuleConfig {
            name: name.into(),
            metric: "m".into(),
            service: None,
            comparator: comparator.into(),
            threshold: 1.0,
            agg: agg.into(),
            window_secs: 300,
            enabled: true,
            match_attrs: None,
            exclude: None,
            rate: None,
            for_secs: None,
        }
    }

    #[test]
    fn validate_whitelists_fields() {
        assert!(validate(&cfg("ok", "gt", "avg")).is_ok());
        assert!(validate(&cfg("bad-cmp", "eq", "avg")).is_err());
        assert!(validate(&cfg("bad-agg", "gt", "median")).is_err());
        assert!(validate(&cfg("", "gt", "avg")).is_err()); // empty name
    }

    #[test]
    fn validate_for_secs_range() {
        let mut r = cfg("dwell", "gt", "avg");
        r.for_secs = None;
        assert!(validate(&r).is_ok()); // absent = single-breach, always fine
        r.for_secs = Some(300);
        assert!(validate(&r).is_ok());
        r.for_secs = Some(0);
        assert!(validate(&r).is_err()); // 0 is not a valid dwell — use absence instead
        r.for_secs = Some(-1);
        assert!(validate(&r).is_err());
        r.for_secs = Some(MAX_FOR_SECS);
        assert!(validate(&r).is_ok()); // the ceiling itself is allowed
        r.for_secs = Some(MAX_FOR_SECS + 1);
        assert!(validate(&r).is_err()); // must stay well under raw retention
    }

    #[test]
    fn config_parses_for_secs() {
        let r = &serde_json::from_str::<Vec<RuleConfig>>(
            r#"[{"name":"r","metric":"m","comparator":"gt","threshold":1,"for_secs":300}]"#,
        )
        .unwrap()[0];
        assert_eq!(r.for_secs, Some(300));
    }

    fn rule() -> AlertRule {
        AlertRule {
            id: 1,
            name: "pod memory > 80%".into(),
            metric: "container.memory.usage".into(),
            service: Some("api".into()),
            comparator: "gt".into(),
            threshold: 80.0,
            agg: "max".into(),
            window_secs: 300,
            match_attrs: None,
            exclude_attrs: None,
            rate: None,
            for_secs: None,
        }
    }

    #[test]
    fn email_subject_states() {
        assert_eq!(
            email_subject("pod memory > 80%", "firing"),
            "[watcher] FIRING: pod memory > 80%"
        );
        assert_eq!(
            email_subject("disk full", "resolved"),
            "[watcher] RESOLVED: disk full"
        );
    }

    #[test]
    fn email_body_includes_condition_and_value() {
        let b = email_body(&rule(), "firing", Some(87.5));
        assert!(b.contains("Alert: pod memory > 80%"));
        assert!(b.contains("State: firing"));
        assert!(b.contains("max(container.memory.usage) gt 80 over 300s"));
        assert!(b.contains("(service api)"));
        assert!(b.contains("Observed: 87.5"));
    }

    #[test]
    fn email_body_handles_no_data_and_no_service() {
        let mut r = rule();
        r.service = None;
        let b = email_body(&r, "resolved", None);
        assert!(b.contains("Observed: no data"));
        assert!(!b.contains("(service"));
    }

    #[test]
    fn agg_expr_whitelist() {
        assert_eq!(agg_expr("avg"), "avg(value)");
        assert_eq!(agg_expr("max"), "max(value)");
        assert_eq!(agg_expr("min"), "min(value)");
        assert_eq!(agg_expr("sum"), "sum(value)");
        assert_eq!(agg_expr("last"), "(array_agg(value ORDER BY time DESC))[1]");
        // Anything unexpected falls back to avg rather than interpolating it.
        assert_eq!(agg_expr("'; DROP TABLE metrics; --"), "avg(value)");
    }

    #[test]
    fn breached_gt() {
        assert!(breached(Some(10.0), "gt", 5.0));
        assert!(!breached(Some(5.0), "gt", 5.0)); // strict
        assert!(!breached(Some(1.0), "gt", 5.0));
    }

    #[test]
    fn breached_lt() {
        assert!(breached(Some(1.0), "lt", 5.0));
        assert!(!breached(Some(5.0), "lt", 5.0));
        assert!(!breached(Some(10.0), "lt", 5.0));
    }

    #[test]
    fn breached_no_data_never_fires() {
        assert!(!breached(None, "gt", 5.0));
        assert!(!breached(None, "lt", 5.0));
        // Unknown comparator never fires.
        assert!(!breached(Some(10.0), "eq", 5.0));
    }

    #[test]
    fn config_parses_match_exclude_rate() {
        let rules: Vec<RuleConfig> = serde_json::from_str(
            r#"[{"name":"r","metric":"m","comparator":"gt","threshold":1,
                 "match":{"owner":"Deployment"},"exclude":{"owner":"Job"},"rate":true}]"#,
        )
        .unwrap();
        let r = &rules[0];
        assert_eq!(r.match_attrs.as_ref().unwrap()["owner"], "Deployment");
        assert_eq!(r.exclude.as_ref().unwrap()["owner"], "Job");
        assert_eq!(r.rate, Some(true));
    }

    #[test]
    fn config_defaults_new_fields_to_none() {
        // A pre-JEF-426 rule (none of the new keys) parses with them all absent.
        let r = &serde_json::from_str::<Vec<RuleConfig>>(
            r#"[{"name":"r","metric":"m","comparator":"gt","threshold":1}]"#,
        )
        .unwrap()[0];
        assert!(r.match_attrs.is_none());
        assert!(r.exclude.is_none());
        assert!(r.rate.is_none());
        assert!(r.for_secs.is_none());
    }

    #[test]
    fn attr_predicate_normalizes_empty_to_none() {
        use super::attr_predicate;
        // Absent and empty both mean "no filter" — never a `{}` that `@>` treats as
        // always-true (which would make an `exclude` suppress every series).
        assert!(attr_predicate(&None).is_none());
        assert!(attr_predicate(&Some(serde_json::Map::new())).is_none());
        let mut m = serde_json::Map::new();
        m.insert("pod".into(), "a".into());
        assert_eq!(attr_predicate(&Some(m)).unwrap()["pod"], "a");
    }

    #[test]
    fn eval_sql_plain_binds_predicates_and_window() {
        let sql = eval_sql("avg", false);
        // match/exclude ride as bound JSONB operands, guarded so a NULL disables them.
        assert!(sql.contains("$4::jsonb IS NULL OR attributes @> $4"));
        assert!(sql.contains("$5::jsonb IS NULL OR NOT (attributes @> $5)"));
        assert!(sql.contains("make_interval(secs => $3)"));
        assert!(sql.contains("avg(value)"));
        // No rate machinery in the plain path.
        assert!(!sql.contains("lag(value)"));
    }

    // A webhook receiver that accepts the TCP connection but never sends a byte of
    // response — the exact "slow/unresponsive receiver" that used to stall `notify`
    // for ~11.5s. The bounded client's 10s request timeout must make `notify` give
    // up well within the margin rather than blocking on the OS TCP timeout.
    #[tokio::test]
    async fn notify_gives_up_on_a_hung_webhook_within_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept forever, holding each connection open without ever responding.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock); // keep it alive; never write back
            }
        });

        let client = build_webhook_client().unwrap();
        let url = format!("http://{addr}/hook");
        let rule = rule();

        let start = std::time::Instant::now();
        // No mailer — this exercises the webhook sink in isolation.
        notify(
            &client,
            Some(url.as_str()),
            None,
            &rule,
            "firing",
            Some(87.5),
        )
        .await;
        let elapsed = start.elapsed();

        // The call must complete (it returns unit even on error, logging the failure)
        // bounded by the 10s request timeout — never the multi-second-to-minutes OS
        // TCP timeout. Generous 15s ceiling keeps it non-flaky under load.
        assert!(
            elapsed < std::time::Duration::from_secs(15),
            "notify() should give up on a hung webhook within the bounded timeout, took {elapsed:?}"
        );
    }

    #[test]
    fn eval_sql_rate_differences_reset_safe() {
        let sql = eval_sql("max", true);
        // Per-series differencing ordered by time, per-second, reset → 0 not a spike.
        assert!(sql.contains("value - lag(value) OVER w"));
        assert!(sql.contains("PARTITION BY service, attributes ORDER BY time"));
        assert!(sql.contains("CASE WHEN dt > 0 AND dv >= 0 THEN dv / dt ELSE 0 END"));
        // The predicate filters still apply before differencing.
        assert!(sql.contains("attributes @> $4"));
        assert!(sql.contains("max(value)"));
    }
}
