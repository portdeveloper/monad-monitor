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
    /// `None` when the scrape did not carry the metric, or carried a value that
    /// is not a reading. Zero is a reading several of these can genuinely give,
    /// so it cannot stand in for "unread": a node at genesis, an idle mempool
    /// and a node with no upstream validators all report a real 0.
    pub block_num: Option<u64>,
    pub tx_commits: Option<u64>,
    /// Milliseconds, or 0 when the counter it belongs to did not arrive. TPS is
    /// a rate over that pair, so neither half is useful without the other.
    pub tx_commits_timestamp_ms: u64,
    /// `None` when the scrape did not carry `monad_peer_disc_num_peers`, or
    /// carried a value that would not parse. Zero is a reading a connected node
    /// can genuinely give, so reporting an unread field as one would raise a
    /// low-peer alert against a count nobody took.
    pub peer_count: Option<u64>,
    pub statesync_progress: Option<u64>,
    pub statesync_target: Option<u64>,
    // New metrics
    pub uptime_us: Option<u64>,
    pub latency_p99_ms: Option<u64>,
    pub pending_txs: Option<u64>,
    pub upstream_validators: Option<u64>,
}

impl PrometheusMetrics {
    /// How far a statesync has got, as a percentage.
    ///
    /// A node that is not statesyncing reports neither gauge, and that has
    /// always meant "nothing left to sync" here. An unread gauge is not the
    /// same thing: a progress figure with no target is a node that IS syncing
    /// and whose target did not arrive, so it must not report completion. Only
    /// the absence of both, or a target of zero, is evidence of a synced node.
    pub fn sync_percentage(&self) -> f64 {
        match (self.statesync_progress, self.statesync_target) {
            (None, None) | (_, Some(0)) => 100.0,
            (Some(progress), Some(target)) => (progress as f64 / target as f64) * 100.0,
            // One half read and the other not: there is a statesync in flight
            // and no way to say how far along. Not synced is the safe direction
            // for a monitor.
            (Some(_), None) | (None, Some(_)) => 0.0,
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
                    if let Some(block_num) = count(value) {
                        metrics.block_num = Some(block_num);
                    }
                }
                "monad_execution_ledger_num_tx_commits" => {
                    // The timestamp rides with the counter: TPS is a rate over
                    // that pair, so keeping one without the other would date a
                    // count that never came with it.
                    // The pair moves together, and a later line that is not a
                    // reading must not strand the timestamp of one that was.
                    if let Some(tx_commits) = count(value) {
                        metrics.tx_commits = Some(tx_commits);
                        metrics.tx_commits_timestamp_ms = timestamp;
                    }
                }
                "monad_peer_disc_num_peers" => {
                    if let Some(peer_count) = count(value) {
                        metrics.peer_count = Some(peer_count);
                    }
                }
                "monad_statesync_progress_estimate" => {
                    if let Some(statesync_progress) = count(value) {
                        metrics.statesync_progress = Some(statesync_progress);
                    }
                }
                "monad_statesync_last_target" => {
                    if let Some(statesync_target) = count(value) {
                        metrics.statesync_target = Some(statesync_target);
                    }
                }
                "monad_total_uptime_us" => {
                    if let Some(uptime_us) = count(value) {
                        metrics.uptime_us = Some(uptime_us);
                    }
                }
                "monad_bft_raptorcast_udp_secondary_broadcast_latency_p99_ms" => {
                    if let Some(latency_p99_ms) = count(value) {
                        metrics.latency_p99_ms = Some(latency_p99_ms);
                    }
                }
                "monad_bft_txpool_pool_tracked_txs" => {
                    if let Some(pending_txs) = count(value) {
                        metrics.pending_txs = Some(pending_txs);
                    }
                }
                "monad_peer_disc_num_upstream_validators" => {
                    if let Some(upstream_validators) = count(value) {
                        metrics.upstream_validators = Some(upstream_validators);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(metrics)
}

/// A counter or gauge read off the wire, or `None` when the number was not one.
///
/// Prometheus carries every value as a float, so a line can parse cleanly and
/// still not be a reading. `as u64` cannot fail: it turns `NaN` and any negative
/// into 0, saturates an infinity to `u64::MAX`, and truncates `1.5` to 1, so
/// each of those lands in the struct looking measured. The node keeps its own
/// metrics in a `HashMap<&'static str, u64>`, so a value that is not whole, not
/// finite, negative or past the end of the type is not one it can export.
fn count(value: f64) -> Option<u64> {
    // `is_finite` is deliberately explicit rather than load-bearing: no input can
    // reach the arms behind it, since `-Inf` is already negative and both `NaN`
    // and `+Inf` have a `fract()` of `NaN`, which is not equal to 0.0. Rejecting
    // them only as a side effect of a float comparison would be a trap for the
    // next reader. `-0.0` is a zero reading rather than a negative one, so the
    // sign test is a comparison, not `is_sign_negative`.
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return None;
    }
    // 2^64, the first f64 past `u64::MAX`. Without this the cast saturates and
    // hands back `u64::MAX` as though it had been measured; for the peer count
    // that value also overflows the trend arithmetic in `state.rs`.
    const ABOVE_U64_MAX: f64 = 18_446_744_073_709_551_616.0;
    if value >= ABOVE_U64_MAX {
        return None;
    }
    Some(value as u64)
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

        assert_eq!(metrics.block_num, Some(41929095));
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

        assert_eq!(metrics.block_num, Some(41929095));
    }

    #[test]
    fn test_parse_metric_line() {
        let line = r#"monad_execution_ledger_block_num{job="test"} 4.1929095e+07 1765694534456"#;
        let (name, value, ts) = parse_metric_line(line).unwrap();
        assert_eq!(name, "monad_execution_ledger_block_num");
        assert_eq!(value as u64, 41929095);
        assert_eq!(ts, 1765694534456);
    }

    #[test]
    fn a_missing_peer_metric_is_unknown_not_zero() {
        // The scrape succeeded, it simply did not carry this field. Everything
        // else in the body has to survive that.
        let m = parse_metrics("monad_execution_ledger_block_num 100\n").expect("parse");

        assert_eq!(m.peer_count, None);
        assert_eq!(m.block_num, Some(100));
    }

    #[test]
    fn a_malformed_peer_value_is_unknown_not_zero() {
        // parse_metric_line rejects the whole line, which used to leave the
        // field at its struct default and read as a measured zero.
        let m = parse_metrics(
            "monad_execution_ledger_block_num 100\nmonad_peer_disc_num_peers not_a_number\n",
        )
        .expect("parse");

        assert_eq!(m.peer_count, None);
        assert_eq!(m.block_num, Some(100));
    }

    #[test]
    fn a_peer_value_that_is_not_a_count_is_unknown_not_a_reading() {
        // parse_metric_line only asks that the text parse as f64, and Prometheus
        // carries every value as a float. These all parse, and `as u64` gave each
        // of them a plausible-looking count: NaN and the negatives became 0, the
        // same reading a node with no peers gives; +Inf saturated to u64::MAX,
        // which reads as healthy and overflows the peer-trend arithmetic; 1.5
        // truncated to 1, which reads as a low-peer node.
        for value in [
            "NaN",
            "nan",
            "inf",
            "+Inf",
            "-Inf",
            "-1",
            "-0.5",
            "1.5",
            "0.1",
            // 2^64 and past it: the cast saturates rather than refusing.
            "18446744073709551616",
            "1e20",
            // u64::MAX itself, which no f64 can hold: the text rounds to 2^64 on
            // the way in, so it arrives indistinguishable from the row above.
            "18446744073709551615",
        ] {
            let body = format!(
                "monad_execution_ledger_block_num 100\nmonad_peer_disc_num_peers {}\n",
                value
            );
            let m = parse_metrics(&body).expect("parse");

            assert_eq!(m.peer_count, None, "{:?} was stored as a count", value);
            // The rest of the scrape is still good; one bad line is not a failed
            // scrape.
            assert_eq!(m.block_num, Some(100), "{:?} lost the block height", value);
        }
    }

    #[test]
    fn a_whole_nonnegative_peer_count_is_still_a_reading() {
        // The other side of the check: the values that ARE counts must survive
        // it, including a zero, a signed zero and one written as a float.
        for (value, expected) in [
            ("0", 0u64),
            ("-0", 0),
            ("0.0", 0),
            ("1", 1),
            ("12", 12),
            ("12.0", 12),
            ("1e2", 100),
            // Large but exactly representable, so it survives the f64 the metric
            // line is carried in. 2^53 is where that stops being true in general.
            ("9007199254740992", 9_007_199_254_740_992),
        ] {
            let body = format!("monad_peer_disc_num_peers {}\n", value);
            let m = parse_metrics(&body).expect("parse");
            assert_eq!(m.peer_count, Some(expected), "{:?} was refused", value);
        }
    }

    /// Every field filled by casting a scraped float, and how to read it back.
    /// They all go through the same check, so the table is the test.
    type Field = (&'static str, fn(&PrometheusMetrics) -> Option<u64>);

    const CAST_FIELDS: [Field; 9] = [
        ("monad_execution_ledger_block_num", |m| m.block_num),
        ("monad_peer_disc_num_peers", |m| m.peer_count),
        ("monad_execution_ledger_num_tx_commits", |m| m.tx_commits),
        ("monad_statesync_progress_estimate", |m| {
            m.statesync_progress
        }),
        ("monad_statesync_last_target", |m| m.statesync_target),
        ("monad_total_uptime_us", |m| m.uptime_us),
        (
            "monad_bft_raptorcast_udp_secondary_broadcast_latency_p99_ms",
            |m| m.latency_p99_ms,
        ),
        ("monad_bft_txpool_pool_tracked_txs", |m| m.pending_txs),
        ("monad_peer_disc_num_upstream_validators", |m| {
            m.upstream_validators
        }),
    ];

    #[test]
    fn an_unread_statesync_gauge_reads_as_nothing_left_to_sync() {
        // A node that is not statesyncing does not report a target, which has
        // always meant "synced" here -- the field used to default to 0 and hit
        // the same branch. Making it optional must not turn every healthy node
        // into one stuck at 0%.
        let nothing = PrometheusMetrics::default();
        assert_eq!(nothing.sync_percentage(), 100.0);
        assert!(nothing.is_synced());

        let explicit_zero = PrometheusMetrics {
            statesync_target: Some(0),
            ..Default::default()
        };
        assert_eq!(explicit_zero.sync_percentage(), 100.0);

        // A real target still divides, and a node mid-sync is not synced.
        let syncing = PrometheusMetrics {
            statesync_progress: Some(500),
            statesync_target: Some(1000),
            ..Default::default()
        };
        assert_eq!(syncing.sync_percentage(), 50.0);
        assert!(!syncing.is_synced());

        // Either half read without the other is a sync in flight that cannot be
        // measured. Both directions take the safe answer, and in particular a
        // progress reading with an unread target must NOT report completion --
        // that is a wedged gauge on a node that is demonstrably syncing.
        for (progress, target) in [(None, Some(1000)), (Some(500), None)] {
            let half = PrometheusMetrics {
                statesync_progress: progress,
                statesync_target: target,
                ..Default::default()
            };
            assert_eq!(half.sync_percentage(), 0.0, "{:?}/{:?}", progress, target);
            assert!(
                !half.is_synced(),
                "{:?}/{:?} claimed synced",
                progress,
                target
            );
        }
    }

    #[test]
    fn a_value_that_is_not_a_reading_is_not_stored() {
        // `as u64` cannot fail, so each of these used to land in the struct
        // looking measured: NaN and the negatives as 0, the infinities and
        // anything past the type as u64::MAX, a fraction truncated. The
        // spellings matter as much as the values -- `1e309` carries none of the
        // word in its text and still arrives as an infinity, so a check written
        // against `inf` would let it through.
        for (metric, field) in CAST_FIELDS {
            for value in [
                "NaN",
                "nan",
                "-1",
                "-0.5",
                "1.5",
                "0.1",
                "inf",
                "Inf",
                "INF",
                "+inf",
                "infinity",
                "Infinity",
                "-inf",
                "1e309",
                "1e20",
                "18446744073709551616",
                // u64::MAX itself: no f64 holds it, so the text rounds to 2^64
                // on the way in and arrives indistinguishable from the row above.
                "18446744073709551615",
            ] {
                // The witness has to be a metric this row is not testing, now
                // that every field goes through the same check.
                let (witness, witness_read): (&str, fn(&PrometheusMetrics) -> Option<u64>) =
                    if metric == "monad_execution_ledger_block_num" {
                        ("monad_peer_disc_num_peers", |m| m.peer_count)
                    } else {
                        ("monad_execution_ledger_block_num", |m| m.block_num)
                    };
                let body = format!("{} 100\n{} {}\n", witness, metric, value);
                let m = parse_metrics(&body).expect("parse");

                assert_eq!(field(&m), None, "{} {:?} was stored", metric, value);
                // One line that is not a reading is not a failed scrape.
                assert_eq!(
                    witness_read(&m),
                    Some(100),
                    "{} {:?} cost the rest of the scrape",
                    metric,
                    value
                );
            }
        }
    }

    #[test]
    fn a_real_reading_is_stored() {
        // The check must not swallow values that ARE readings, including a zero
        // and the exponent and trailing-point forms an exporter may use.
        for (metric, field) in CAST_FIELDS {
            for (value, expected) in [
                ("0", 0u64),
                ("-0", 0),
                ("7", 7),
                ("1e2", 100),
                ("5.", 5),
                // Large but exactly representable, so it survives the f64 the
                // metric line is carried in. 2^53 is where that stops in general.
                ("9007199254740992", 9_007_199_254_740_992),
                // The largest integer an f64 holds below 2^64. If ABOVE_U64_MAX
                // were set even slightly low, a legal large gauge would be
                // refused and nothing else here would notice.
                ("18446744073709549568", 18_446_744_073_709_549_568),
            ] {
                let body = format!("{} {}\n", metric, value);
                let m = parse_metrics(&body).expect("parse");

                assert_eq!(
                    field(&m),
                    Some(expected),
                    "{} {:?} was refused",
                    metric,
                    value
                );
            }
        }
    }

    #[test]
    fn a_zero_reading_is_stored_and_not_merely_the_default() {
        // Some(0) and None are now distinguishable, so this no longer has to
        // prove a zero was taken rather than defaulted. It still guards the
        // check itself: `value <= 0.0` is one character away from `< 0.0`.
        for (metric, field) in CAST_FIELDS {
            let body = format!("{} 7\n{} 0\n", metric, metric);
            let m = parse_metrics(&body).expect("parse");

            assert_eq!(field(&m), Some(0), "{} refused a real zero", metric);
        }
    }

    #[test]
    fn a_line_that_is_not_a_reading_does_not_erase_one_that_was() {
        // Duplicate names are not supposed to happen, but a proxy or a
        // federation endpoint can produce them. A later line that is refused
        // must leave the earlier reading standing rather than blanking it --
        // and for tx_commits it must not strand the timestamp either, or the
        // JSON shows a time against a count that is not there.
        // Every refusal class, not just NaN: a parser that special-cased one of
        // them and clobbered on the rest would pass a NaN-only version of this.
        for (metric, field) in CAST_FIELDS {
            for bad in ["NaN", "-1", "1.5", "+Inf", "1e309", "18446744073709551616"] {
                // A witness the row is not testing, so a refused line that
                // reached past its own field would show up here.
                let (witness, witness_read): (&str, fn(&PrometheusMetrics) -> Option<u64>) =
                    if metric == "monad_peer_disc_num_peers" {
                        ("monad_execution_ledger_block_num", |m| m.block_num)
                    } else {
                        ("monad_peer_disc_num_peers", |m| m.peer_count)
                    };
                let body = format!("{} 42\n{} 7\n{} {}\n", witness, metric, metric, bad);
                let m = parse_metrics(&body).expect("parse");

                assert_eq!(field(&m), Some(7), "{} erased by {:?}", metric, bad);
                assert_eq!(
                    witness_read(&m),
                    Some(42),
                    "{} {:?} took the witness with it",
                    metric,
                    bad
                );
            }

            // The other order: refused first, then a real reading, which wins.
            let body = format!("{} NaN\n{} 7\n", metric, metric);
            let m = parse_metrics(&body).expect("parse");
            assert_eq!(
                field(&m),
                Some(7),
                "{} lost a reading that came after a refusal",
                metric
            );
        }

        // The refused line carries its OWN timestamp, so this also pins that a
        // kept count is not re-dated by a reading that was thrown away.
        let m = parse_metrics(
            "monad_execution_ledger_num_tx_commits 99 2000\n\
             monad_execution_ledger_num_tx_commits NaN 9999\n",
        )
        .expect("parse");
        assert_eq!(m.tx_commits, Some(99));
        assert_eq!(m.tx_commits_timestamp_ms, 2000, "the pair came apart");
    }

    #[test]
    fn the_commit_timestamp_does_not_outlive_the_count_it_came_with() {
        // TPS is a rate over the counter and its timestamp. Taking the timestamp
        // of a reading that was refused would date a count that never came with
        // it, and a zero counter against a fresh timestamp reads as a node that
        // has committed nothing.
        let refused =
            parse_metrics("monad_execution_ledger_num_tx_commits NaN 2000\n").expect("parse");
        assert_eq!(refused.tx_commits, None);
        assert_eq!(refused.tx_commits_timestamp_ms, 0);

        let read = parse_metrics("monad_execution_ledger_num_tx_commits 99 2000\n").expect("parse");
        assert_eq!(read.tx_commits, Some(99));
        assert_eq!(read.tx_commits_timestamp_ms, 2000);
    }

    #[test]
    fn a_real_zero_peer_count_is_still_a_reading() {
        let m = parse_metrics("monad_peer_disc_num_peers 0\n").expect("parse");
        assert_eq!(m.peer_count, Some(0));
    }

    #[test]
    fn a_positive_peer_count_is_read() {
        let m = parse_metrics("monad_peer_disc_num_peers 12\n").expect("parse");
        assert_eq!(m.peer_count, Some(12));
    }
}
