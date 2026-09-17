use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Serialize;
use tokio::time::timeout;

/// How long a single scrape may take, headers and body together. The TUI awaits
/// each scrape before the next tick, so an endpoint that accepts a connection and
/// then goes quiet used to park the polling task for good.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Metrics fetched from Prometheus endpoint
#[derive(Debug, Clone, Default, Serialize)]
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
    fetch_timeout: Duration,
}

impl MetricsClient {
    pub fn new(endpoint: &str) -> Self {
        Self::with_timeout(endpoint, FETCH_TIMEOUT)
    }

    /// Same client with an explicit deadline, so tests do not have to wait out
    /// `FETCH_TIMEOUT` to observe a stalled endpoint.
    fn with_timeout(endpoint: &str, fetch_timeout: Duration) -> Self {
        Self {
            client: Client::new(),
            endpoint: endpoint.to_string(),
            fetch_timeout,
        }
    }

    /// The URL being scraped, so an error can name the endpoint it tried.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn fetch(&self) -> Result<PrometheusMetrics> {
        // Dropping the request future on expiry closes the connection, so the next
        // tick scrapes instead of queueing behind a request that will never answer.
        timeout(self.fetch_timeout, self.scrape())
            .await
            .map_err(|_| {
                anyhow!(
                    "Metrics scrape timed out after {}s",
                    self.fetch_timeout.as_secs_f32()
                )
            })?
    }

    async fn scrape(&self) -> Result<PrometheusMetrics> {
        let body = self
            .client
            .get(&self.endpoint)
            .send()
            .await
            .context("Failed to fetch metrics")?
            // A non-2xx response carries no metrics, and its body parses to a default
            // `PrometheusMetrics` — so without this the caller cannot tell a failed
            // scrape from a node reporting zeroes.
            .error_for_status()?
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

    /// Short enough to keep the stalled-endpoint tests fast, long enough that a
    /// loopback response is never mistaken for a hang.
    const TEST_TIMEOUT: Duration = Duration::from_millis(250);

    /// Serve exactly one HTTP response on a loopback port, then return its URL.
    /// A plain `std::net::TcpListener` on a background thread keeps this test free of
    /// new dependencies: the crate has no `[dev-dependencies]` and tokio is built
    /// without the `net` feature, and CONTRIBUTING asks to keep dependencies minimal.
    fn serve_once(response: String) -> String {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("read local addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request first: replying before the client has finished
                // writing can surface as a broken pipe instead of the status we set.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/metrics")
    }

    /// Serve connections that never finish a response: each one is accepted, its
    /// request is read, then the stream is parked in `held` so the socket stays
    /// open. `partial` is what the endpoint manages to write before going quiet —
    /// `None` stalls on the headers, a header block with an unfilled
    /// `Content-Length` stalls on the body. Once `stalls` connections have been
    /// parked, the next one is answered with `response`.
    fn serve_stalling(stalls: usize, partial: Option<String>, response: String) -> String {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("read local addr");
        std::thread::spawn(move || {
            let mut stalled = 0usize;
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                if stalled < stalls {
                    stalled += 1;
                    if let Some(partial) = &partial {
                        let _ = stream.write_all(partial.as_bytes());
                        let _ = stream.flush();
                    }
                    // Parking the stream rather than dropping it is the point: a
                    // closed socket would fail the scrape at once, which is not
                    // the hang this guards against.
                    held.push(stream);
                    continue;
                }
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/metrics")
    }

    fn http_response(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn http_error_status_is_not_a_successful_scrape() {
        // Without a status check the error body simply carries no known metrics, so
        // `parse_metrics` returns a default `PrometheusMetrics` and the caller sees
        // `Ok(zeroes)` — a failed scrape that is indistinguishable from an idle node.
        let endpoint = serve_once(http_response(
            "500 Internal Server Error",
            "internal server error",
        ));

        let err = MetricsClient::new(&endpoint)
            .fetch()
            .await
            .expect_err("a 500 must not read as a successful scrape");

        // Assert on the status itself rather than on the message: the URL in the text
        // carries an ephemeral port, and a port such as 45001 contains "500", so a
        // substring check could pass without the status ever being looked at.
        let status = err
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .expect("the failure should carry the HTTP status it saw");
        assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn a_successful_response_is_still_parsed() {
        // The status check must not cost us the happy path.
        let endpoint = serve_once(http_response(
            "200 OK",
            "monad_execution_ledger_block_num{job=\"test\"} 4.1929095e+07 1765694534456\n",
        ));

        let metrics = MetricsClient::new(&endpoint)
            .fetch()
            .await
            .expect("a 200 with a valid body is a successful scrape");

        assert_eq!(metrics.block_num, 41929095);
    }

    #[tokio::test]
    async fn a_stalled_response_header_ends_the_scrape() {
        // The polling task awaits each scrape, so an endpoint that accepts the
        // connection and then says nothing used to stop metrics for the session.
        let endpoint = serve_stalling(1, None, http_response("200 OK", ""));

        let err = MetricsClient::with_timeout(&endpoint, TEST_TIMEOUT)
            .fetch()
            .await
            .expect_err("an endpoint that never answers is not a successful scrape");

        assert!(
            err.to_string().contains("timed out"),
            "the failure should read as a timeout, got: {err}"
        );
    }

    #[tokio::test]
    async fn a_stalled_response_body_ends_the_scrape() {
        // Headers alone are not the finish line: the body is read separately, and a
        // `Content-Length` the endpoint never fills hangs just as the headers do.
        let endpoint = serve_stalling(
            1,
            Some(
                "HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nmonad_peer_disc_num_peers 7"
                    .to_string(),
            ),
            http_response("200 OK", ""),
        );

        let err = MetricsClient::with_timeout(&endpoint, TEST_TIMEOUT)
            .fetch()
            .await
            .expect_err("a body that never arrives is not a successful scrape");

        assert!(
            err.to_string().contains("timed out"),
            "the failure should read as a timeout, got: {err}"
        );
    }

    #[tokio::test]
    async fn polling_recovers_once_the_endpoint_answers_again() {
        // A timeout has to leave the client usable, otherwise the panel stays on the
        // error for as long as the process runs.
        let endpoint = serve_stalling(
            1,
            None,
            http_response(
                "200 OK",
                "monad_execution_ledger_block_num{job=\"test\"} 4.1929095e+07 1765694534456\n",
            ),
        );
        let client = MetricsClient::with_timeout(&endpoint, TEST_TIMEOUT);

        client.fetch().await.expect_err("the first scrape stalls");
        let metrics = client
            .fetch()
            .await
            .expect("the next scrape should reach a healthy endpoint");

        assert_eq!(metrics.block_num, 41929095);
    }

    #[test]
    fn test_parse_metric_line() {
        let line = r#"monad_execution_ledger_block_num{job="test"} 4.1929095e+07 1765694534456"#;
        let (name, value, ts) = parse_metric_line(line).unwrap();
        assert_eq!(name, "monad_execution_ledger_block_num");
        assert_eq!(value as u64, 41929095);
        assert_eq!(ts, 1765694534456);
    }
}
