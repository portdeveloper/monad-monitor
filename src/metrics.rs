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
    /// `None` when the scrape did not carry `monad_peer_disc_num_peers`, or
    /// carried a value that would not parse. Zero is a reading a connected node
    /// can genuinely give, so reporting an unread field as one would raise a
    /// low-peer alert against a count nobody took.
    pub peer_count: Option<u64>,
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
                    if let Some(block_num) = count(value) {
                        metrics.block_num = block_num;
                    }
                }
                "monad_execution_ledger_num_tx_commits" => {
                    // The timestamp rides with the counter: TPS is a rate over
                    // that pair, so keeping one without the other would date a
                    // count that never came with it.
                    if let Some(tx_commits) = count(value) {
                        metrics.tx_commits = tx_commits;
                        metrics.tx_commits_timestamp_ms = timestamp;
                    }
                }
                "monad_peer_disc_num_peers" => {
                    metrics.peer_count = count(value);
                }
                "monad_statesync_progress_estimate" => {
                    if let Some(statesync_progress) = count(value) {
                        metrics.statesync_progress = statesync_progress;
                    }
                }
                "monad_statesync_last_target" => {
                    if let Some(statesync_target) = count(value) {
                        metrics.statesync_target = statesync_target;
                    }
                }
                "monad_total_uptime_us" => {
                    if let Some(uptime_us) = count(value) {
                        metrics.uptime_us = uptime_us;
                    }
                }
                "monad_bft_raptorcast_udp_secondary_broadcast_latency_p99_ms" => {
                    if let Some(latency_p99_ms) = count(value) {
                        metrics.latency_p99_ms = latency_p99_ms;
                    }
                }
                "monad_bft_txpool_pool_tracked_txs" => {
                    if let Some(pending_txs) = count(value) {
                        metrics.pending_txs = pending_txs;
                    }
                }
                "monad_peer_disc_num_upstream_validators" => {
                    if let Some(upstream_validators) = count(value) {
                        metrics.upstream_validators = upstream_validators;
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

    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Short enough to keep the stalled-endpoint tests fast, long enough that a
    /// loopback response is never mistaken for a hang.
    const TEST_TIMEOUT: Duration = Duration::from_millis(250);

    /// The assertion's own bound, deliberately written as its own literal rather
    /// than derived from `TEST_TIMEOUT` or `FETCH_TIMEOUT`. These tests exist to
    /// prove the client stops on its own; an outer bound computed from the very
    /// deadline under test would move with it, so removing that deadline would
    /// widen the assertion instead of failing it. Generous on purpose: it is not
    /// a performance budget, it is the line between "slow" and "never".
    const ASSERTION_DEADLINE: Duration = Duration::from_secs(5);

    /// Await a scrape that is supposed to end by itself, and fail the test if it
    /// does not. Without this the stalled-endpoint tests hang until the CI job's
    /// own timeout, which reports as a job that ran out of time rather than as
    /// the regression it is.
    async fn before_deadline<F: Future>(what: &str, scrape: F) -> F::Output {
        match timeout(ASSERTION_DEADLINE, scrape).await {
            Ok(finished) => finished,
            Err(_) => panic!(
                "{what}: the scrape did not end within {ASSERTION_DEADLINE:?}, so the client is \
                 no longer bounding a stalled endpoint"
            ),
        }
    }

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
    fn serve_stalling(stalls: usize, partial: Option<String>, response: String) -> StallingEndpoint {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("read local addr");
        // Polled rather than blocked on, so the thread can notice the guard going
        // away. `incoming()` parks inside accept() and would keep this thread and
        // every parked socket alive for the rest of the test binary.
        listener
            .set_nonblocking(true)
            .expect("poll the listener so the fixture can be shut down");
        let stop = Arc::new(AtomicBool::new(false));
        let watch = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut stalled = 0usize;
            let mut held = Vec::new();
            while !watch.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                // The listener polls; a connection that arrived should be read the
                // ordinary way, and never past the point where the test is over.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(ASSERTION_DEADLINE));
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
            // Dropping `held` here is what finally closes the parked sockets.
        });
        StallingEndpoint {
            url: format!("http://{addr}/metrics"),
            stop,
            thread: Some(thread),
        }
    }

    /// Owns the stalling fixture so the test owns its lifetime. Dropping it stops
    /// the accept loop and closes every parked socket, and drop runs on the way
    /// out of a failed assertion too, which is the case that used to leak: a
    /// panicking test left its thread accepting and its sockets open.
    struct StallingEndpoint {
        url: String,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl StallingEndpoint {
        fn url(&self) -> &str {
            &self.url
        }
    }

    impl Drop for StallingEndpoint {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                // Joining rather than detaching: it is what makes the close ordered,
                // so a later test cannot meet a socket this one was still holding.
                let _ = thread.join();
            }
        }
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

        let err = before_deadline(
            "a header stall",
            MetricsClient::with_timeout(endpoint.url(), TEST_TIMEOUT).fetch(),
        )
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

        let err = before_deadline(
            "a body stall",
            MetricsClient::with_timeout(endpoint.url(), TEST_TIMEOUT).fetch(),
        )
        .await
        .expect_err("a body that never arrives is not a successful scrape");

        assert!(
            err.to_string().contains("timed out"),
            "the failure should read as a timeout, got: {err}"
        );
    }

    #[test]
    fn the_stalling_fixture_closes_when_the_test_is_over() {
        // The cleanup the two tests above depend on, asserted directly rather than
        // assumed: while the guard is alive the port accepts, and once it is
        // dropped the listener is gone and the parked socket with it. Without this
        // a future change could quietly go back to detaching the thread, and every
        // test here would still pass while leaking a listener per run.
        let endpoint = serve_stalling(1, None, http_response("200 OK", ""));
        let addr = endpoint
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/metrics")
            .to_string();

        std::net::TcpStream::connect(&addr).expect("the fixture accepts while the test holds it");
        drop(endpoint);

        // The accept loop polls on a 5 ms tick, so give it a moment to notice the
        // flag before reading anything into a connection that races it.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            std::net::TcpStream::connect(&addr).is_err(),
            "the fixture kept listening on {addr} after the test that owned it ended"
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
        let client = MetricsClient::with_timeout(endpoint.url(), TEST_TIMEOUT);

        before_deadline("the stalled first scrape", client.fetch())
            .await
            .expect_err("the first scrape stalls");
        let metrics = before_deadline("the recovery scrape", client.fetch())
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

    #[test]
    fn a_missing_peer_metric_is_unknown_not_zero() {
        // The scrape succeeded, it simply did not carry this field. Everything
        // else in the body has to survive that.
        let m = parse_metrics("monad_execution_ledger_block_num 100\n").expect("parse");

        assert_eq!(m.peer_count, None);
        assert_eq!(m.block_num, 100);
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
        assert_eq!(m.block_num, 100);
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
            assert_eq!(m.block_num, 100, "{:?} lost the block height", value);
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
    type Field = (&'static str, fn(&PrometheusMetrics) -> u64);

    const CAST_FIELDS: [Field; 8] = [
        ("monad_execution_ledger_block_num", |m| m.block_num),
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
                let body = format!("monad_peer_disc_num_peers 12\n{} {}\n", metric, value);
                let m = parse_metrics(&body).expect("parse");

                assert_eq!(field(&m), 0, "{} {:?} was stored", metric, value);
                // One line that is not a reading is not a failed scrape.
                assert_eq!(
                    m.peer_count,
                    Some(12),
                    "{} {:?} cost the rest",
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

                assert_eq!(field(&m), expected, "{} {:?} was refused", metric, value);
            }
        }
    }

    #[test]
    fn a_zero_reading_is_stored_and_not_merely_the_default() {
        // These fields are plain u64, so a refused value and a real zero both
        // leave 0 behind and the table above cannot tell them apart. Writing a
        // non-zero first and then a zero in the same body is what proves the
        // zero was taken: if the check ever started refusing 0 -- `value <= 0.0`
        // is one character away -- the earlier value would survive here.
        for (metric, field) in CAST_FIELDS {
            let body = format!("{} 7\n{} 0\n", metric, metric);
            let m = parse_metrics(&body).expect("parse");

            assert_eq!(field(&m), 0, "{} refused a real zero", metric);
        }
    }

    #[test]
    fn the_commit_timestamp_does_not_outlive_the_count_it_came_with() {
        // TPS is a rate over the counter and its timestamp. Taking the timestamp
        // of a reading that was refused would date a count that never came with
        // it, and a zero counter against a fresh timestamp reads as a node that
        // has committed nothing.
        let refused =
            parse_metrics("monad_execution_ledger_num_tx_commits NaN 2000\n").expect("parse");
        assert_eq!(refused.tx_commits, 0);
        assert_eq!(refused.tx_commits_timestamp_ms, 0);

        let read = parse_metrics("monad_execution_ledger_num_tx_commits 99 2000\n").expect("parse");
        assert_eq!(read.tx_commits, 99);
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
