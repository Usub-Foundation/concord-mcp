//! Background job: expires leases and settles negotiations whose deadline passed.
//! Safe to run on every hub instance at once (namespaces are taken with SKIP LOCKED).

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::hub::Hub;
use crate::ops::{negotiate, tasks};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickReport {
    pub leases_expired: usize,
    pub negotiations_settled: usize,
    pub namespaces_skipped: usize,
}

impl Hub {
    /// One sweep over every namespace with something due at `clock.now()`.
    pub async fn tick(&self) -> Result<TickReport> {
        let now = self.now();
        let due: Vec<(String,)> = sqlx::query_as(
            "SELECT namespace_id FROM tasks
              WHERE lease_until < $1 AND status IN ('claimed', 'in_progress', 'blocked')
             UNION
             SELECT namespace_id FROM negotiations WHERE status <> 'closed' AND deadline <= $1",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        let mut report = TickReport::default();
        for (ns,) in due {
            let Some(mut op) = self.try_begin(&ns, "system").await? else {
                report.namespaces_skipped += 1;
                continue;
            };
            report.leases_expired += tasks::expire_leases(&mut op).await?;
            report.negotiations_settled += negotiate::settle_due(&mut op).await?;
            self.commit(op).await?;
        }
        sqlx::query("DELETE FROM mcp_sessions WHERE updated_at < now() - interval '7 days'")
            .execute(&self.pool)
            .await?;
        Ok(report)
    }
}

pub async fn run(hub: Arc<Hub>, every: Duration, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(every) => {}
        }
        match hub.tick().await {
            Ok(r) if r.leases_expired + r.negotiations_settled > 0 => {
                tracing::info!(leases = r.leases_expired, negotiations = r.negotiations_settled, "sweep");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "sweep failed"),
        }
    }
}
