//! Background retention: periodically delete telemetry older than the configured window.

use sqlx::PgPool;
use std::time::Duration;

/// Runs forever; ticks immediately, then hourly.
pub async fn run(pool: PgPool, days: i32) {
    if days <= 0 {
        tracing::info!("retention disabled (WATCHER_RETENTION_DAYS <= 0)");
        return;
    }
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        for (table, col) in [
            ("spans", "start_time"),
            ("logs", "time"),
            ("metrics", "time"),
        ] {
            let sql =
                format!("DELETE FROM {table} WHERE {col} < now() - make_interval(days => $1)");
            match sqlx::query(&sql).bind(days).execute(&pool).await {
                Ok(r) if r.rows_affected() > 0 => {
                    tracing::info!("retention: pruned {} rows from {table}", r.rows_affected())
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("retention sweep of {table} failed: {e}"),
            }
        }
    }
}
