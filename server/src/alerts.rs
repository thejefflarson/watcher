//! Alerting: evaluate threshold rules against recent metric points on a timer.
//! Each breach opens an `alert_events` row (resolved when the value recovers),
//! logs the transition, and optionally POSTs a webhook. Storage + log are always
//! on; the webhook fires only when `WATCHER_ALERT_WEBHOOK` is set.

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::Serialize;
use sqlx::PgPool;
use std::time::Duration;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// A rule as stored. Shared by the evaluator and the CRUD API.
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

/// Whether `value` breaches the rule. No data in the window (`None`) never fires.
fn breached(value: Option<f64>, comparator: &str, threshold: f64) -> bool {
    match (value, comparator) {
        (Some(v), "gt") => v > threshold,
        (Some(v), "lt") => v < threshold,
        _ => false,
    }
}

/// Runs forever; ticks immediately, then every `interval_secs`.
pub async fn run(
    pool: PgPool,
    webhook: Option<String>,
    email: Option<EmailConfig>,
    interval_secs: u64,
) {
    let client = reqwest::Client::new();
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
    let client = reqwest::Client::new();
    evaluate(pool, webhook, None, &client).await
}

async fn evaluate(
    pool: &PgPool,
    webhook: Option<&str>,
    mailer: Option<&Mailer>,
    client: &reqwest::Client,
) -> anyhow::Result<()> {
    let rules = sqlx::query_as::<_, AlertRule>(
        "SELECT id, name, metric, service, comparator, threshold, agg, window_secs
         FROM alert_rules WHERE enabled = TRUE",
    )
    .fetch_all(pool)
    .await?;

    for rule in rules {
        let sql = format!(
            "SELECT {} FROM metrics
             WHERE name = $1 AND ($2::text IS NULL OR service = $2)
               AND value IS NOT NULL
               AND time >= now() - make_interval(secs => $3)",
            agg_expr(&rule.agg)
        );
        let value: Option<f64> = sqlx::query_scalar(&sql)
            .bind(&rule.metric)
            .bind(&rule.service)
            .bind(rule.window_secs as f64)
            .fetch_one(pool)
            .await?;

        let breached = breached(value, &rule.comparator, rule.threshold);

        let open: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM alert_events WHERE rule_id = $1 AND resolved_at IS NULL",
        )
        .bind(rule.id)
        .fetch_optional(pool)
        .await?;

        match (breached, open) {
            (true, None) => {
                sqlx::query("INSERT INTO alert_events (rule_id, value) VALUES ($1, $2)")
                    .bind(rule.id)
                    .bind(value)
                    .execute(pool)
                    .await?;
                tracing::warn!(
                    "alert '{}' firing: {} {} {} (value {:?})",
                    rule.name,
                    rule.metric,
                    rule.comparator,
                    rule.threshold,
                    value
                );
                notify(client, webhook, mailer, &rule, "firing", value).await;
            }
            (false, Some(id)) => {
                sqlx::query("UPDATE alert_events SET resolved_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(pool)
                    .await?;
                tracing::info!("alert '{}' resolved (value {:?})", rule.name, value);
                notify(client, webhook, mailer, &rule, "resolved", value).await;
            }
            _ => {}
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
    use super::{agg_expr, breached, email_body, email_subject, AlertRule};

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
}
