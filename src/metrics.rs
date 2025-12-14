use anyhow::{Context, Result};
use reqwest::Client;

/// Metrics fetched from Prometheus endpoint
#[derive(Debug, Clone, Default)]
pub struct PrometheusMetrics {
    pub block_num: u64,
    pub tx_commits: u64,
    pub tx_commits_timestamp_ms: u64,
    pub peer_count: u64,
    pub statesync_progress: u64,
    pub statesync_target: u64,
    // New metrics
    pub uptime_us: u64,
    pub latency_p99_ms: u64,
    pub pending_txs: u64,
    pub upstream_validators: u64,
}

impl PrometheusMetrics {
    pub fn sync_percentage(&self) -> f64 {
        if self.statesync_target == 0 {
            100.0
        } else {
            (self.statesync_progress as f64 / self.statesync_target as f64) * 100.0
        }
    }

    pub fn is_synced(&self) -> bool {
        self.sync_percentage() >= 99.99
    }
}

pub struct MetricsClient {
    client: Client,
    endpoint: String,
}

impl MetricsClient {
    pub fn new(endpoint: &str) -> Self {
        Self {
            client: Client::new(),
            endpoint: endpoint.to_string(),
        }
    }

    pub async fn fetch(&self) -> Result<PrometheusMetrics> {
        let body = self
            .client
            .get(&self.endpoint)
            .send()
            .await
            .context("Failed to fetch metrics")?
            .text()
            .await
            .context("Failed to read metrics body")?;

        parse_metrics(&body)
    }
}

fn parse_metrics(body: &str) -> Result<PrometheusMetrics> {
    let mut metrics = PrometheusMetrics::default();

    for line in body.lines() {
        // Skip comments and empty lines
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Parse metric lines: metric_name{labels} value timestamp
        // or: metric_name value timestamp
        if let Some((name, value, timestamp)) = parse_metric_line(line) {
            match name {
                "monad_execution_ledger_block_num" => {
                    metrics.block_num = value as u64;
                }
                "monad_execution_ledger_num_tx_commits" => {
                    metrics.tx_commits = value as u64;
                    metrics.tx_commits_timestamp_ms = timestamp;
                }
                "monad_peer_disc_num_peers" => {
                    metrics.peer_count = value as u64;
                }
                "monad_statesync_progress_estimate" => {
                    metrics.statesync_progress = value as u64;
                }
                "monad_statesync_last_target" => {
                    metrics.statesync_target = value as u64;
                }
                "monad_total_uptime_us" => {
                    metrics.uptime_us = value as u64;
                }
                "monad_bft_raptorcast_udp_secondary_broadcast_latency_p99_ms" => {
                    metrics.latency_p99_ms = value as u64;
                }
                "monad_bft_txpool_pool_tracked_txs" => {
                    metrics.pending_txs = value as u64;
                }
                "monad_peer_disc_num_upstream_validators" => {
                    metrics.upstream_validators = value as u64;
                }
                _ => {}
            }
        }
    }

    Ok(metrics)
}

fn parse_metric_line(line: &str) -> Option<(&str, f64, u64)> {
    // Handle lines with labels: metric_name{label="value"} 123.45 1234567890
    // Handle lines without labels: metric_name 123.45 1234567890

    let (name, rest) = if let Some(brace_pos) = line.find('{') {
        let name = &line[..brace_pos];
        // Find closing brace and skip to value
        let after_brace = line.find('}')?;
        (name, line[after_brace + 1..].trim())
    } else {
        // No labels, split on first whitespace
        let mut parts = line.splitn(2, char::is_whitespace);
        let name = parts.next()?;
        let rest = parts.next()?.trim();
        (name, rest)
    };

    // Parse value and optional timestamp
    let mut parts = rest.split_whitespace();
    let value: f64 = parts.next()?.parse().ok()?;
    let timestamp: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    Some((name, value, timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_metric_line() {
        let line = r#"monad_execution_ledger_block_num{job="test"} 4.1929095e+07 1765694534456"#;
        let (name, value, ts) = parse_metric_line(line).unwrap();
        assert_eq!(name, "monad_execution_ledger_block_num");
        assert_eq!(value as u64, 41929095);
        assert_eq!(ts, 1765694534456);
    }
}
