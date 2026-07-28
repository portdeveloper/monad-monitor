//! Headless JSON output for scripting and exporters.
//!
//! `--json` prints one snapshot as a single JSON object on stdout and exits.
//! `--json --watch <secs>` emits one object per interval as NDJSON.
//! The TUI path is untouched; this module reuses the same fetch code.

use std::io::{self, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use tokio::time::timeout;

use crate::metrics::{MetricsClient, PrometheusMetrics};
use crate::system::{SystemClient, SystemData};

/// How long to wait for the metrics endpoint before treating it as unreachable
const METRICS_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for the system probes (monad-mpt, systemctl, external RPC)
const SYSTEM_TIMEOUT: Duration = Duration::from_secs(5);
/// Gap between the two counter samples used for the one-shot TPS value
const TPS_SAMPLE_GAP: Duration = Duration::from_secs(1);
/// Minimum time between system probe refreshes in watch mode (matches the TUI)
const SYSTEM_REFRESH: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
pub struct Options {
    pub watch: Option<u64>,
}

/// Parse the headless flags out of argv.
///
/// Returns Ok(None) when --json is absent so the TUI path runs unchanged.
/// Unknown flags stay ignored, same as the TUI today; broader flag handling
/// is out of scope here.
pub fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut json = false;
    let mut watch = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--watch" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--watch needs a value in seconds".to_string())?;
                let secs: u64 = value.parse().map_err(|_| {
                    format!("--watch needs a whole number of seconds, got {:?}", value)
                })?;
                if secs == 0 {
                    return Err("--watch interval must be at least 1 second".to_string());
                }
                watch = Some(secs);
            }
            _ => {}
        }
    }

    if watch.is_some() && !json {
        return Err("--watch requires --json".to_string());
    }
    if json {
        Ok(Some(Options { watch }))
    } else {
        Ok(None)
    }
}

#[derive(Debug, Serialize)]
pub struct Services {
    pub bft: bool,
    pub execution: bool,
    pub rpc: bool,
}

/// One snapshot of the node, serialized as a single JSON object.
///
/// The top-level keys are the numbers the issue names, for `jq` one-liners.
/// The full parsed structs are nested under `metrics` and `system` so
/// exporters get every field without a second data path. The metrics-derived
/// fields are null on watch lines where the metrics endpoint was down.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub timestamp: u64,
    pub block_height: Option<u64>,
    pub peer_count: Option<u64>,
    pub tps: Option<f64>,
    pub latency_p99_ms: Option<u64>,
    pub sync_pct: Option<f64>,
    pub finalized_lag: u64,
    pub cpu_usage_pct: f64,
    pub memory_used_pct: f64,
    pub disk_used_pct: f64,
    pub services: Services,
    pub metrics: Option<PrometheusMetrics>,
    pub system: SystemData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn build_snapshot(
    timestamp: u64,
    metrics: Option<&PrometheusMetrics>,
    tps: Option<f64>,
    system: &SystemData,
    error: Option<String>,
) -> Snapshot {
    Snapshot {
        timestamp,
        block_height: metrics.map(|m| m.block_num),
        peer_count: metrics.map(|m| m.peer_count),
        tps,
        latency_p99_ms: metrics.map(|m| m.latency_p99_ms),
        sync_pct: metrics.map(|m| m.sync_percentage()),
        finalized_lag: system.finalized_lag(),
        cpu_usage_pct: system.cpu_usage_pct,
        memory_used_pct: system.memory_used_pct,
        disk_used_pct: system.disk_used_pct,
        services: Services {
            bft: system.service_bft,
            execution: system.service_execution,
            rpc: system.service_rpc,
        },
        metrics: metrics.cloned(),
        system: system.clone(),
        error,
    }
}

/// TPS between two scrapes of the tx commit counter.
///
/// A single scrape cannot give TPS because the counter is cumulative.
/// Returns None when the counter timestamps are missing, time did not
/// advance or the counter reset (node restart).
pub fn tps_between(prev: &PrometheusMetrics, cur: &PrometheusMetrics) -> Option<f64> {
    if prev.tx_commits_timestamp_ms == 0 || cur.tx_commits_timestamp_ms == 0 {
        return None;
    }
    if cur.tx_commits_timestamp_ms <= prev.tx_commits_timestamp_ms {
        return None;
    }
    if cur.tx_commits < prev.tx_commits {
        return None;
    }
    let tx_delta = (cur.tx_commits - prev.tx_commits) as f64;
    let time_delta_ms = (cur.tx_commits_timestamp_ms - prev.tx_commits_timestamp_ms) as f64;
    Some(tx_delta / time_delta_ms * 1000.0)
}

pub async fn run(options: Options, metrics_endpoint: &str, network: &str) -> Result<()> {
    match options.watch {
        None => run_once(metrics_endpoint, network).await,
        Some(secs) => run_watch(secs, metrics_endpoint, network).await,
    }
}

/// One snapshot, one JSON line, then exit.
///
/// Fails fast with a nonzero exit when the metrics endpoint is unreachable so
/// shell scripts can alert on it. The system probes degrade to defaults when
/// they fail; the node metrics are the part scripts key on.
async fn run_once(metrics_endpoint: &str, network: &str) -> Result<()> {
    let metrics_client = MetricsClient::new(metrics_endpoint);
    let system_client = SystemClient::new(network);

    let first = fetch_metrics(&metrics_client, metrics_endpoint).await?;

    // TPS needs a counter delta, so take a second sample after a short gap.
    // The system probes run during the same gap.
    let (system, second) = tokio::join!(timeout(SYSTEM_TIMEOUT, system_client.fetch()), async {
        tokio::time::sleep(TPS_SAMPLE_GAP).await;
        fetch_metrics(&metrics_client, metrics_endpoint).await
    },);
    let second = second?;
    let system = match system {
        Ok(Ok(data)) => data,
        _ => SystemData::default(),
    };

    let tps = tps_between(&first, &second);
    let snapshot = build_snapshot(unix_now(), Some(&second), tps, &system, None);
    write_line(&snapshot)?;
    Ok(())
}

/// Emit one NDJSON line per interval until interrupted.
///
/// Keeps running when the node goes down; those lines carry the system stats
/// plus an `error` field and null metrics so a consumer can alert without
/// losing the stream. TPS comes from consecutive samples, so the first line
/// has tps null.
async fn run_watch(secs: u64, metrics_endpoint: &str, network: &str) -> Result<()> {
    let metrics_client = MetricsClient::new(metrics_endpoint);
    let system_client = SystemClient::new(network);
    let mut ticker = tokio::time::interval(Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut prev: Option<PrometheusMetrics> = None;
    let mut system = SystemData::default();
    let mut system_fetched_at: Option<Instant> = None;

    loop {
        ticker.tick().await;

        // The system probes spawn processes and dial an external RPC, so
        // refresh them at most every SYSTEM_REFRESH like the TUI does
        let refresh_due = match system_fetched_at {
            None => true,
            Some(at) => at.elapsed() >= SYSTEM_REFRESH,
        };
        if refresh_due {
            if let Ok(Ok(data)) = timeout(SYSTEM_TIMEOUT, system_client.fetch()).await {
                system = data;
            }
            system_fetched_at = Some(Instant::now());
        }

        let snapshot = match fetch_metrics(&metrics_client, metrics_endpoint).await {
            Ok(cur) => {
                let tps = prev.as_ref().and_then(|p| tps_between(p, &cur));
                let snap = build_snapshot(unix_now(), Some(&cur), tps, &system, None);
                prev = Some(cur);
                snap
            }
            Err(e) => build_snapshot(unix_now(), None, None, &system, Some(format!("{:#}", e))),
        };

        if !write_line(&snapshot)? {
            // stdout is gone (for example piped into head), stop quietly
            return Ok(());
        }
    }
}

async fn fetch_metrics(client: &MetricsClient, endpoint: &str) -> Result<PrometheusMetrics> {
    match timeout(METRICS_TIMEOUT, client.fetch()).await {
        Ok(result) => result.with_context(|| format!("failed to fetch metrics from {}", endpoint)),
        Err(_) => Err(anyhow!(
            "timed out after {}s fetching metrics from {}",
            METRICS_TIMEOUT.as_secs(),
            endpoint
        )),
    }
}

/// Write one snapshot as a JSON line and flush so pipes see it immediately.
/// Returns false when stdout is closed (broken pipe).
fn write_line(snapshot: &Snapshot) -> Result<bool> {
    let line = serde_json::to_string(snapshot)?;
    let mut out = io::stdout();
    match writeln!(out, "{}", line).and_then(|_| out.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_no_flags_runs_tui() {
        assert_eq!(parse_args(&args(&[])), Ok(None));
        assert_eq!(parse_args(&args(&["--theme", "gray"])), Ok(None));
    }

    #[test]
    fn parse_json_one_shot() {
        assert_eq!(
            parse_args(&args(&["--json"])),
            Ok(Some(Options { watch: None }))
        );
    }

    #[test]
    fn parse_json_watch() {
        assert_eq!(
            parse_args(&args(&["--json", "--watch", "5"])),
            Ok(Some(Options { watch: Some(5) }))
        );
    }

    #[test]
    fn parse_watch_errors() {
        assert!(parse_args(&args(&["--watch", "5"])).is_err());
        assert!(parse_args(&args(&["--json", "--watch"])).is_err());
        assert!(parse_args(&args(&["--json", "--watch", "0"])).is_err());
        assert!(parse_args(&args(&["--json", "--watch", "abc"])).is_err());
    }

    fn sample_metrics() -> PrometheusMetrics {
        PrometheusMetrics {
            block_num: 41929095,
            tx_commits: 1_000_000,
            tx_commits_timestamp_ms: 1_765_694_534_000,
            peer_count: 45,
            statesync_progress: 100,
            statesync_target: 0,
            uptime_us: 86_400_000_000,
            latency_p99_ms: 12,
            pending_txs: 30,
            upstream_validators: 5,
        }
    }

    fn sample_system() -> SystemData {
        SystemData {
            disk_used_pct: 6.11,
            history_latest: 41933100,
            latest_finalized: 41933098,
            service_bft: true,
            service_execution: true,
            service_rpc: false,
            memory_used_pct: 45.2,
            cpu_usage_pct: 23.4,
            ..Default::default()
        }
    }

    #[test]
    fn tps_from_two_samples() {
        let prev = sample_metrics();
        let mut cur = sample_metrics();
        cur.tx_commits = 1_001_000;
        cur.tx_commits_timestamp_ms = prev.tx_commits_timestamp_ms + 1000;
        assert_eq!(tps_between(&prev, &cur), Some(1000.0));
    }

    #[test]
    fn tps_none_without_timestamps() {
        let mut prev = sample_metrics();
        let cur = sample_metrics();
        prev.tx_commits_timestamp_ms = 0;
        assert_eq!(tps_between(&prev, &cur), None);
    }

    #[test]
    fn tps_none_when_time_does_not_advance() {
        let prev = sample_metrics();
        let cur = sample_metrics();
        assert_eq!(tps_between(&prev, &cur), None);
    }

    #[test]
    fn tps_none_on_counter_reset() {
        let prev = sample_metrics();
        let mut cur = sample_metrics();
        cur.tx_commits = 100;
        cur.tx_commits_timestamp_ms = prev.tx_commits_timestamp_ms + 1000;
        assert_eq!(tps_between(&prev, &cur), None);
    }

    #[test]
    fn snapshot_json_shape() {
        let metrics = sample_metrics();
        let system = sample_system();
        let snap = build_snapshot(1_765_694_535, Some(&metrics), Some(1234.5), &system, None);
        let value = serde_json::to_value(&snap).unwrap();

        assert_eq!(value["timestamp"], 1_765_694_535u64);
        assert_eq!(value["block_height"], 41929095u64);
        assert_eq!(value["peer_count"], 45);
        assert_eq!(value["tps"], 1234.5);
        assert_eq!(value["latency_p99_ms"], 12);
        // statesync_target 0 means synced, existing sync_percentage() rule
        assert_eq!(value["sync_pct"], 100.0);
        assert_eq!(value["finalized_lag"], 2);
        assert_eq!(value["cpu_usage_pct"], 23.4);
        assert_eq!(value["memory_used_pct"], 45.2);
        assert_eq!(value["disk_used_pct"], 6.11);
        assert_eq!(value["services"]["bft"], true);
        assert_eq!(value["services"]["rpc"], false);
        // full structs nested for exporters
        assert_eq!(value["metrics"]["pending_txs"], 30);
        assert_eq!(value["system"]["history_latest"], 41933100u64);
        // no error key on a clean snapshot
        assert!(value.get("error").is_none());
    }

    #[test]
    fn snapshot_json_when_metrics_down() {
        let system = sample_system();
        let snap = build_snapshot(
            1_765_694_535,
            None,
            None,
            &system,
            Some("failed to fetch metrics from http://localhost:8889/metrics".to_string()),
        );
        let value = serde_json::to_value(&snap).unwrap();

        assert_eq!(value["block_height"], serde_json::Value::Null);
        assert_eq!(value["tps"], serde_json::Value::Null);
        assert_eq!(value["metrics"], serde_json::Value::Null);
        // system stats still present so a consumer sees the host is alive
        assert_eq!(value["cpu_usage_pct"], 23.4);
        assert!(value["error"].as_str().unwrap().contains("8889"));
    }
}
