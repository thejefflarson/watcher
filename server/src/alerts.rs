//! Alerting: evaluate threshold rules against recent metric points on a timer.
//! Each breach opens an `alert_events` row (resolved when the value recovers),
//! logs the transition, and optionally POSTs a webhook. Storage + log are always
//! on; the webhook fires only when `WATCHER_ALERT_WEBHOOK` is set.

use serde::Serialize;
use sqlx::PgPool;
use std::time::Duration;

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
pub async fn run(pool: PgPool, webhook: Option<String>, interval_secs: u64) {
    let client = reqwest::Client::new();
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs.max(5)));
    loop {
        interval.tick().await;
        if let Err(e) = evaluate(&pool, webhook.as_deref(), &client).await {
            tracing::warn!("alert evaluation failed: {e}");
        }
    }
}

/// A single evaluation pass — opens/resolves events for every enabled rule.
/// Exposed for tests and one-shot use; `run` calls it on a timer.
pub async fn evaluate_once(pool: &PgPool, webhook: Option<&str>) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    evaluate(pool, webhook, &client).await
}

async fn evaluate(
    pool: &PgPool,
    webhook: Option<&str>,
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
                notify(client, webhook, &rule, "firing", value).await;
            }
            (false, Some(id)) => {
                sqlx::query("UPDATE alert_events SET resolved_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(pool)
                    .await?;
                tracing::info!("alert '{}' resolved (value {:?})", rule.name, value);
                notify(client, webhook, &rule, "resolved", value).await;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn notify(
    client: &reqwest::Client,
    webhook: Option<&str>,
    rule: &AlertRule,
    state: &str,
    value: Option<f64>,
) {
    let Some(url) = webhook else { return };
    let payload = WebhookPayload {
        rule: &rule.name,
        metric: &rule.metric,
        service: rule.service.as_deref(),
        state,
        value,
        threshold: rule.threshold,
        comparator: &rule.comparator,
    };
    if let Err(e) = client.post(url).json(&payload).send().await {
        tracing::warn!("alert webhook POST failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{agg_expr, breached};

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
