//! Headless JSON snapshot mode.
//!
//! Prints the same numbers the TUI shows, but as a JSON object on stdout so
//! they can be used from scripts, cron checks and exporters. This reuses the
//! metrics/RPC/system data path and the `AppState` reducer rather than a
//! parallel one: we drive `AppState` with a couple of readings and serialize a
//! flat view of it.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tokio::time::timeout;

use crate::config::Config;
use crate::metrics::{MetricsClient, PrometheusMetrics};
use crate::rpc::RpcClient;
use crate::state::AppState;
use crate::system::{SystemClient, SystemData};

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// A flat, script-friendly view of the current node state. The raw metrics and
/// system structs are flattened in, and the values the TUI derives from them
/// (block height, TPS, finalized lag, sync status) are lifted to the top level
/// so `monad-monitor --json | jq .block_height` works.
#[derive(Serialize)]
pub struct Snapshot {
    pub timestamp_ms: u64,
    pub network: String,
    pub node_reachable: bool,
    /// `null` when neither the RPC nor the metrics scrape reported a height, so
    /// a script can tell a node at genesis from one that has not been read.
    pub block_height: Option<u64>,
    pub synced: bool,
    pub sync_percentage: f64,
    pub tps: f64,
    pub tps_peak: f64,
    pub peer_health: String,
    /// `null` when the monad-mpt read did not produce the two heights it is
    /// derived from, so a script can tell a healthy zero lag from no reading.
    pub finalized_lag: Option<u64>,
    /// `null` when the public node could not be reached, so a script can tell
    /// "no difference" apart from "no comparison".
    pub block_difference: Option<i64>,
    pub all_services_running: bool,
    /// `null` when the node never produced a readable gas price, so a script
    /// can tell a healthy zero from no reading.
    pub gas_price_gwei: Option<f64>,
    pub client_version: String,
    #[serde(flatten)]
    pub metrics: PrometheusMetrics,
    #[serde(flatten)]
    pub system: SystemData,
}

impl Snapshot {
    pub fn from_state(state: &AppState, network: &str, node_reachable: bool) -> Self {
        let block_height = state.block_height();
        Snapshot {
            timestamp_ms: now_ms(),
            network: network.to_string(),
            node_reachable,
            block_height,
            synced: state.metrics.is_synced(),
            sync_percentage: state.metrics.sync_percentage(),
            tps: state.tps,
            tps_peak: state.tps_peak,
            peer_health: state.peer_health().to_string(),
            finalized_lag: state.system.finalized_lag(),
            block_difference: state.system.block_difference(block_height),
            all_services_running: state.system.all_services_running(),
            gas_price_gwei: state.rpc_data.gas_price_gwei,
            client_version: state.rpc_data.client_version.clone(),
            metrics: state.metrics.clone(),
            system: state.system.clone(),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pull one reading from each source into `state`. Returns whether the node is
/// reachable, meaning at least one of the local metrics or RPC endpoints
/// answered. A failing system fetch (monad-mpt / systemctl) does not by itself
/// mean the node is down, so it is not counted here.
async fn collect(
    state: &mut AppState,
    metrics: &MetricsClient,
    system: &mut SystemClient,
    rpc: &RpcClient,
) -> bool {
    let metrics_ok = if let Ok(Ok(m)) = timeout(FETCH_TIMEOUT, metrics.fetch()).await {
        state.update_metrics(m);
        true
    } else {
        false
    };

    if let Ok(Ok(s)) = timeout(FETCH_TIMEOUT, system.fetch()).await {
        state.update_system(s);
    }

    let rpc_ok = if let Ok(Ok(r)) = timeout(FETCH_TIMEOUT, rpc.fetch_once()).await {
        state.update_rpc(r);
        true
    } else {
        // Mark the source down, as the TUI's event loop would: block_height
        // then follows the still-answering metrics poll instead of freezing on
        // the last height this endpoint gave.
        state.set_ws_disconnected("RPC endpoint did not answer".to_string());
        false
    };

    metrics_ok || rpc_ok
}

/// Where a watch line's reading comes from.
///
/// Production reads the node. A test scripts the readings, which is the only way
/// to assert "exactly n lines" from both ends: the count the writer received and
/// the number of readings the loop asked for.
trait Samples {
    async fn next_sample(&mut self) -> (Snapshot, bool);
}

/// The node itself, carrying the state the TPS rate is computed from across
/// readings.
struct NodeSamples<'a> {
    state: AppState,
    metrics: &'a MetricsClient,
    system: &'a mut SystemClient,
    rpc: &'a RpcClient,
    network: &'a str,
}

impl Samples for NodeSamples<'_> {
    async fn next_sample(&mut self) -> (Snapshot, bool) {
        let reachable = collect(&mut self.state, self.metrics, self.system, self.rpc).await;
        (
            Snapshot::from_state(&self.state, self.network, reachable),
            reachable,
        )
    }
}

/// What a failed write or flush means for the exit code.
///
/// A closed pipe is the reader going away, which the operator asked for, so it
/// stays a quiet exit 0. Anything else (a full disk, a revoked descriptor, a
/// writer that cannot flush) is an output that was not delivered, and reporting
/// success over it would hide a failed capture.
fn output_failed(e: std::io::Error) -> Result<i32> {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        Ok(0)
    } else {
        Err(anyhow::Error::new(e).context("writing JSON watch output"))
    }
}

/// Emit NDJSON on every tick, stopping after `count` lines when one is given.
///
/// `count = None` keeps the behaviour this had before it was bounded: stream
/// until the reader goes away. A closed pipe is still a quiet exit 0 in both
/// cases — the operator who pressed `head -1` did not ask for a failure. Any
/// other write or flush error is returned, so the process exits nonzero with
/// the cause. A completed bounded capture reports the last reading's
/// reachability instead, matching the one-shot convention, because there the
/// exit status is the answer rather than an interruption; a line only counts
/// once both its write and its flush have succeeded.
async fn emit<S: Samples, W: Write>(
    samples: &mut S,
    out: &mut W,
    period: Duration,
    count: Option<u64>,
) -> Result<i32> {
    // Prime one reading, which is not a sample: nothing is written for it, and a
    // bounded capture of n still emits n lines. It exists so the first emitted
    // line already carries a TPS rate, which a single reading cannot give. The
    // rule lives here rather than at the call site so that one place owns it and
    // a test can hold it to the count.
    samples.next_sample().await;

    let mut ticker = tokio::time::interval(period);
    let mut emitted: u64 = 0;
    loop {
        ticker.tick().await;
        let (snap, reachable) = samples.next_sample().await;
        if let Err(e) = writeln!(out, "{}", serde_json::to_string(&snap)?) {
            return output_failed(e);
        }
        if let Err(e) = out.flush() {
            return output_failed(e);
        }
        emitted += 1;
        if count.is_some_and(|n| emitted >= n) {
            return Ok(if reachable { 0 } else { 1 });
        }
    }
}

/// Run the headless mode. With `watch = None` it prints one snapshot and
/// returns an exit code (0 reachable, 1 unreachable). With `watch = Some(secs)`
/// it prints one JSON object per interval as NDJSON: until interrupted, or until
/// `count` lines have been emitted when a count is given.
pub async fn run(cfg: &Config, watch: Option<u64>, count: Option<u64>) -> Result<i32> {
    let metrics = MetricsClient::new(&cfg.metrics_url);
    let mut system = SystemClient::new(&cfg.resolved_external_rpc_url());
    let rpc = RpcClient::new(&cfg.ws_url);
    let mut state = AppState::new();

    match watch {
        None => {
            // Two readings a second apart so TPS (a rate) is meaningful rather
            // than always zero on a single reading.
            collect(&mut state, &metrics, &mut system, &rpc).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            let reachable = collect(&mut state, &metrics, &mut system, &rpc).await;

            let snap = Snapshot::from_state(&state, &cfg.network, reachable);
            println!("{}", serde_json::to_string(&snap)?);
            Ok(if reachable { 0 } else { 1 })
        }
        Some(secs) => {
            let period = Duration::from_secs(secs.max(1));
            let mut samples = NodeSamples {
                state,
                metrics: &metrics,
                system: &mut system,
                rpc: &rpc,
                network: &cfg.network,
            };
            let mut out = std::io::stdout().lock();
            emit(&mut samples, &mut out, period, count).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::RpcData;

    /// A scripted sample source. Counts what the loop asked for, so "exactly n
    /// lines" is checked from both ends rather than only at the writer.
    struct ScriptedSamples {
        asked: usize,
        reachable: Vec<bool>,
    }

    impl ScriptedSamples {
        fn always(reachable: bool) -> Self {
            Self {
                asked: 0,
                reachable: vec![reachable; 64],
            }
        }
    }

    impl Samples for ScriptedSamples {
        async fn next_sample(&mut self) -> (Snapshot, bool) {
            let reachable = *self.reachable.get(self.asked).unwrap_or(&true);
            self.asked += 1;
            let state = AppState::new();
            (
                Snapshot::from_state(&state, "testnet", reachable),
                reachable,
            )
        }
    }

    /// A writer that fails on its first call, standing in for a pipe whose reader
    /// has gone away.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose `write` fails with the given kind, for output errors that are
    /// not a closed pipe (a full disk, a revoked descriptor).
    struct FailingWrite(std::io::ErrorKind);

    impl Write for FailingWrite {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0, "write failed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer that accepts every byte and then fails to flush them, so the line
    /// was handed over but never delivered.
    struct FailingFlush(std::io::ErrorKind);

    impl Write for FailingFlush {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(self.0, "flush failed"))
        }
    }

    const FAST: Duration = Duration::from_millis(1);

    /// Run `emit` under a deadline.
    ///
    /// A bounded capture that never reaches its count does not fail a plain test —
    /// it hangs, and a hanging test spends the whole CI budget instead of saying
    /// what broke. A mutation that drops the counter's increment found this: the
    /// suite stopped responding rather than going red. The deadline turns that
    /// into a named failure.
    async fn emit_bounded<S: Samples, W: Write>(
        samples: &mut S,
        out: &mut W,
        count: Option<u64>,
    ) -> Result<i32> {
        match timeout(Duration::from_secs(5), emit(samples, out, FAST, count)).await {
            Ok(r) => r,
            Err(_) => panic!("emit did not finish within 5s for count {:?}", count),
        }
    }

    #[tokio::test]
    async fn a_bounded_capture_emits_exactly_the_count_it_was_given() {
        for want in [1usize, 2, 3, 7] {
            let mut samples = ScriptedSamples::always(true);
            let mut out: Vec<u8> = Vec::new();
            let code = emit_bounded(&mut samples, &mut out, Some(want as u64))
                .await
                .unwrap();

            let lines: Vec<&str> = std::str::from_utf8(&out)
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .collect();
            assert_eq!(
                lines.len(),
                want,
                "count {} emitted {} lines",
                want,
                lines.len()
            );
            // One more reading than lines: the priming read is taken and not written.
            assert_eq!(
                samples.asked,
                want + 1,
                "count {} asked for {} readings, expected {} plus the priming one",
                want,
                samples.asked,
                want
            );
            assert_eq!(code, 0);
            // Every line is a whole snapshot, not a fragment of one.
            for l in lines {
                serde_json::from_str::<serde_json::Value>(l).expect("line was not a JSON object");
            }
        }
    }

    #[tokio::test]
    async fn a_completed_capture_reports_the_last_reading_like_the_one_shot_does() {
        // Unreachable on the final reading only: the status answers "what did the
        // last sample see", not "was anything ever reachable".
        let mut samples = ScriptedSamples {
            asked: 0,
            // The first entry is consumed by the priming read; the three after it are
            // the emitted samples.
            reachable: vec![true, true, true, false],
        };
        let mut out: Vec<u8> = Vec::new();
        let code = emit_bounded(&mut samples, &mut out, Some(3)).await.unwrap();
        assert_eq!(code, 1, "a capture ending on an unreachable node exited 0");

        let mut samples = ScriptedSamples {
            asked: 0,
            reachable: vec![false, false, false, true],
        };
        let mut out: Vec<u8> = Vec::new();
        let code = emit_bounded(&mut samples, &mut out, Some(3)).await.unwrap();
        assert_eq!(
            code, 0,
            "a capture ending on a reachable node did not exit 0"
        );
    }

    #[tokio::test]
    async fn a_closed_pipe_still_exits_quietly_bounded_or_not() {
        // The operator who piped into `head -1` did not ask for a failure, and that
        // was true before --count existed. It stays true on both paths.
        for count in [None, Some(5)] {
            let mut samples = ScriptedSamples::always(true);
            let code = emit_bounded(&mut samples, &mut ClosedPipe, count)
                .await
                .unwrap();
            assert_eq!(
                code, 0,
                "count {:?} turned a closed pipe into a failure",
                count
            );
            assert_eq!(
                samples.asked, 2,
                "the loop kept reading after the pipe closed (one priming read, one written)"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_write_that_is_not_a_closed_pipe_is_an_error_bounded_or_not() {
        for count in [None, Some(5)] {
            let mut samples = ScriptedSamples::always(true);
            let err = emit_bounded(
                &mut samples,
                &mut FailingWrite(std::io::ErrorKind::Other),
                count,
            )
            .await
            .expect_err(&format!(
                "count {:?} reported success over a failed write",
                count
            ));
            assert!(
                format!("{:#}", err).contains("writing JSON watch output"),
                "count {:?}: error lost its context: {:#}",
                count,
                err
            );
            assert_eq!(
                samples.asked, 2,
                "count {:?}: the loop kept reading after the output failed",
                count
            );
        }
    }

    #[tokio::test]
    async fn a_failed_flush_is_an_error_even_when_the_last_sample_was_reachable() {
        // --count 1 with a reachable sample used to return that sample's status, 0,
        // although the line never reached its destination.
        for count in [None, Some(1), Some(5)] {
            let mut samples = ScriptedSamples::always(true);
            let err = emit_bounded(
                &mut samples,
                &mut FailingFlush(std::io::ErrorKind::Other),
                count,
            )
            .await
            .expect_err(&format!(
                "count {:?} reported success over a failed flush",
                count
            ));
            assert!(
                format!("{:#}", err).contains("flush failed"),
                "count {:?}: the cause is missing: {:#}",
                count,
                err
            );
            assert_eq!(
                samples.asked, 2,
                "count {:?}: kept reading after the flush failed",
                count
            );
        }
    }

    #[tokio::test]
    async fn a_closed_pipe_on_flush_is_as_quiet_as_on_write() {
        for count in [None, Some(5)] {
            let mut samples = ScriptedSamples::always(true);
            let code = emit_bounded(
                &mut samples,
                &mut FailingFlush(std::io::ErrorKind::BrokenPipe),
                count,
            )
            .await
            .unwrap();
            assert_eq!(
                code, 0,
                "count {:?} turned a closed pipe into a failure",
                count
            );
            assert_eq!(
                samples.asked, 2,
                "count {:?}: kept reading after the pipe closed",
                count
            );
        }
    }

    #[test]
    fn block_height_is_top_level_and_prefers_rpc() {
        let mut state = AppState::new();
        let rpc = RpcData {
            block_number: Some(12345),
            ..Default::default()
        };
        state.update_rpc(rpc);

        let snap = Snapshot::from_state(&state, "testnet", true);
        let v = serde_json::to_value(&snap).unwrap();

        assert_eq!(v["block_height"], 12345);
        assert_eq!(v["network"], "testnet");
        assert_eq!(v["node_reachable"], true);
    }

    #[test]
    fn an_unread_height_serializes_as_null_rather_than_zero() {
        // Neither source has read a height: a fresh state whose RPC reply
        // never parsed a block quantity. A script must see null here, not a
        // node sitting at genesis.
        let mut state = AppState::new();
        state.update_rpc(RpcData::default());

        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert!(
            v["block_height"].is_null(),
            "an unread height reached the snapshot as {}",
            v["block_height"]
        );
        assert!(
            v["block_difference"].is_null(),
            "a difference was computed against a height nobody read"
        );
    }

    #[test]
    fn a_real_zero_height_reaches_the_snapshot_as_zero() {
        // The other side of the same contract: a node at genesis reports
        // zero, and that is a reading. Both branches keep it -- the
        // subscription leading, and the metrics poll after it drops.
        let mut state = AppState::new();
        state.update_rpc(RpcData {
            block_number: Some(0),
            ..Default::default()
        });
        state.update_metrics(PrometheusMetrics {
            block_num: None,
            ..Default::default()
        });

        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert_eq!(
            v["block_height"], 0,
            "a measured zero-height reading was reported as no reading"
        );

        state.set_ws_disconnected("gone".to_string());
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert_eq!(
            v["block_height"], 0,
            "the disconnected branch lost the measured zero"
        );
    }

    #[test]
    fn flattened_metrics_and_system_fields_are_present() {
        let mut state = AppState::new();
        let metrics = PrometheusMetrics {
            peer_count: Some(7),
            ..Default::default()
        };
        state.update_metrics(metrics);
        let system = SystemData {
            history_latest: Some(100),
            latest_finalized: Some(98),
            disk_used_pct: Some(42.5),
            ..Default::default()
        };
        state.update_system(system);

        let snap = Snapshot::from_state(&state, "mainnet", true);
        let v = serde_json::to_value(&snap).unwrap();

        // flattened from PrometheusMetrics / SystemData
        assert_eq!(v["peer_count"], 7);
        assert_eq!(v["disk_used_pct"], 42.5);
        // derived and lifted to the top level
        assert_eq!(v["finalized_lag"], 2);
    }

    #[test]
    fn an_unread_mpt_serializes_as_null_rather_than_zero() {
        let mut state = AppState::new();
        state.update_system(SystemData::default());
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();

        // A script reading these can tell "not measured" from a healthy zero.
        assert!(v["finalized_lag"].is_null());
        assert!(v["disk_used_pct"].is_null());
        assert!(v["latest_finalized"].is_null());

        state.update_system(SystemData {
            disk_used_pct: Some(79.82),
            history_latest: Some(102_735_394),
            latest_finalized: Some(102_735_393),
            ..Default::default()
        });
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert_eq!(v["finalized_lag"], 1);
        assert_eq!(v["disk_used_pct"], 79.82);
    }

    #[test]
    fn an_unread_metric_serializes_as_null_rather_than_zero() {
        // Same contract #37 set for the monad-mpt readings and #40 for the peer
        // count: a script has to be able to tell a real zero from no reading.
        // Several of these can genuinely be zero on a healthy node.
        let state = AppState::new();
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", false)).unwrap();

        for field in [
            "block_num",
            "tx_commits",
            "statesync_progress",
            "statesync_target",
            "uptime_us",
            "latency_p99_ms",
            "pending_txs",
            "upstream_validators",
        ] {
            assert!(
                v[field].is_null(),
                "{} was invented before any scrape",
                field
            );
        }

        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            block_num: Some(0),
            pending_txs: Some(9),
            ..Default::default()
        });
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();

        assert_eq!(v["block_num"], 0, "a real zero stopped being a reading");
        assert_eq!(v["pending_txs"], 9);
        assert!(v["uptime_us"].is_null(), "an unread field became a number");
    }

    #[test]
    fn an_unknown_peer_count_serializes_as_null_rather_than_zero() {
        // A scrape that omitted the metric must not reach a JSON consumer as a
        // node with no peers. A real zero still has to survive as zero.
        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            block_num: Some(100),
            ..Default::default()
        });
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert!(v["peer_count"].is_null());
        assert_eq!(v["block_num"], 100, "the rest of the scrape survives");

        state.update_metrics(PrometheusMetrics {
            peer_count: Some(0),
            ..Default::default()
        });
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert_eq!(v["peer_count"], 0);
    }

    #[test]
    fn an_unreachable_external_node_leaves_the_difference_null() {
        let mut state = AppState::new();
        state.update_system(SystemData {
            external_block: None,
            ..Default::default()
        });
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert!(v["block_difference"].is_null());

        state.update_system(SystemData {
            external_block: Some(120),
            ..Default::default()
        });
        state.update_rpc(RpcData {
            block_number: Some(100),
            ..Default::default()
        });
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert_eq!(v["block_difference"], 20);
    }

    #[test]
    fn an_unread_gas_price_reaches_the_snapshot_as_null_not_zero() {
        // The handshake quantity never parsed, so RpcData stays at its
        // default. A script must be able to tell "no reading" from a real
        // zero gas price.
        let mut state = AppState::new();
        state.update_rpc(RpcData::default());
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert!(
            v["gas_price_gwei"].is_null(),
            "unread gas price must serialize as null, got {}",
            v["gas_price_gwei"]
        );
    }

    #[test]
    fn a_real_zero_gas_price_still_reaches_the_snapshot_as_zero() {
        // The other side: `Some(0.0)` is a measured zero and must not be
        // collapsed into the unknown case.
        let mut state = AppState::new();
        state.update_rpc(RpcData {
            block_number: Some(50),
            gas_price_gwei: Some(0.0),
            ..Default::default()
        });
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert_eq!(v["gas_price_gwei"].as_f64(), Some(0.0));
        assert_eq!(v["block_height"], 50);
    }

    #[test]
    fn a_bad_live_gas_reply_keeps_the_last_reading_the_operator_sees() {
        // Previously measured value first, then a malformed live reply: the
        // snapshot still carries the measurement, not zero and not null.
        use crate::rpc::apply_gas_price;
        use serde_json::json;

        let mut data = RpcData::default();
        assert!(apply_gas_price(&mut data, Some(&json!("0x77359400"))));
        assert!(!apply_gas_price(&mut data, Some(&json!("0xzz"))));

        let mut state = AppState::new();
        state.update_rpc(data);
        let v = serde_json::to_value(Snapshot::from_state(&state, "mainnet", true)).unwrap();
        assert_eq!(v["gas_price_gwei"], 2.0);
    }

    #[test]
    fn unreachable_flag_is_reflected() {
        let state = AppState::new();
        let snap = Snapshot::from_state(&state, "mainnet", false);
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["node_reachable"], false);
    }
}
