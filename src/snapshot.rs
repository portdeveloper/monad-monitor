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
    pub block_height: u64,
    pub synced: bool,
    pub sync_percentage: f64,
    pub tps: f64,
    pub tps_peak: f64,
    pub peer_health: String,
    pub finalized_lag: u64,
    pub block_difference: i64,
    pub all_services_running: bool,
    pub gas_price_gwei: f64,
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
        false
    };

    metrics_ok || rpc_ok
}

/// Run the headless mode. With `watch = None` it prints one snapshot and
/// returns an exit code (0 reachable, 1 unreachable). With `watch = Some(secs)`
/// it prints one JSON object per interval as NDJSON and runs until interrupted.
pub async fn run(
    network: &str,
    metrics_endpoint: &str,
    rpc_endpoint: &str,
    watch: Option<u64>,
) -> Result<i32> {
    let metrics = MetricsClient::new(metrics_endpoint);
    let mut system = SystemClient::new(network);
    let rpc = RpcClient::new(rpc_endpoint);
    let mut state = AppState::new();

    match watch {
        None => {
            // Two readings a second apart so TPS (a rate) is meaningful rather
            // than always zero on a single reading.
            collect(&mut state, &metrics, &mut system, &rpc).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            let reachable = collect(&mut state, &metrics, &mut system, &rpc).await;

            let snap = Snapshot::from_state(&state, network, reachable);
            println!("{}", serde_json::to_string(&snap)?);
            Ok(if reachable { 0 } else { 1 })
        }
        Some(secs) => {
            let period = Duration::from_secs(secs.max(1));
            // Prime one reading so the first emitted line already has a TPS.
            collect(&mut state, &metrics, &mut system, &rpc).await;

            let mut ticker = tokio::time::interval(period);
            let stdout = std::io::stdout();
            loop {
                ticker.tick().await;
                let reachable = collect(&mut state, &metrics, &mut system, &rpc).await;
                let snap = Snapshot::from_state(&state, network, reachable);
                let line = serde_json::to_string(&snap)?;
                let mut handle = stdout.lock();
                // If stdout is gone (piped into a closed reader), stop quietly.
                if writeln!(handle, "{}", line).is_err() {
                    return Ok(0);
                }
                let _ = handle.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::RpcData;

    #[test]
    fn block_height_is_top_level_and_prefers_rpc() {
        let mut state = AppState::new();
        let rpc = RpcData {
            block_number: 12345,
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
    fn flattened_metrics_and_system_fields_are_present() {
        let mut state = AppState::new();
        let metrics = PrometheusMetrics {
            peer_count: 7,
            ..Default::default()
        };
        state.update_metrics(metrics);
        let system = SystemData {
            history_latest: 100,
            latest_finalized: 98,
            disk_used_pct: 42.5,
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
    fn unreachable_flag_is_reflected() {
        let state = AppState::new();
        let snap = Snapshot::from_state(&state, "mainnet", false);
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["node_reachable"], false);
    }
}
