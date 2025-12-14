use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::metrics::PrometheusMetrics;
use crate::rpc::{Block, RpcData};
use crate::system::SystemData;

const TPS_HISTORY_SIZE: usize = 300; // 5 minutes of history (fills wide terminals)
const SAMPLE_HISTORY_SIZE: usize = 10; // Keep last 10 samples for TPS calculation

#[derive(Debug, Clone)]
struct TxSample {
    tx_commits: u64,
    timestamp_ms: u64,
}

pub struct AppState {
    // Current data
    pub metrics: PrometheusMetrics,
    pub rpc_data: RpcData,
    pub system: SystemData,

    // TPS calculation
    tx_samples: VecDeque<TxSample>,
    pub tps: f64,
    pub tps_history: VecDeque<u64>,

    // Timing
    pub last_update: Instant,
    pub last_block_time: Option<Instant>,
    last_block_number: u64,

    // Error tracking
    pub last_error: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            metrics: PrometheusMetrics::default(),
            rpc_data: RpcData::default(),
            system: SystemData::default(),
            tx_samples: VecDeque::with_capacity(SAMPLE_HISTORY_SIZE),
            tps: 0.0,
            tps_history: VecDeque::with_capacity(TPS_HISTORY_SIZE),
            last_update: Instant::now(),
            last_block_time: None,
            last_block_number: 0,
            last_error: None,
        }
    }

    pub fn update_metrics(&mut self, metrics: PrometheusMetrics) {
        // Track new block
        if metrics.block_num > self.last_block_number {
            self.last_block_time = Some(Instant::now());
            self.last_block_number = metrics.block_num;
        }

        // Add TX sample for TPS calculation
        if metrics.tx_commits_timestamp_ms > 0 {
            let sample = TxSample {
                tx_commits: metrics.tx_commits,
                timestamp_ms: metrics.tx_commits_timestamp_ms,
            };

            // Only add if timestamp is newer
            if self
                .tx_samples
                .back()
                .map(|s| sample.timestamp_ms > s.timestamp_ms)
                .unwrap_or(true)
            {
                self.tx_samples.push_back(sample);
                if self.tx_samples.len() > SAMPLE_HISTORY_SIZE {
                    self.tx_samples.pop_front();
                }
            }
        }

        // Calculate TPS from samples
        self.calculate_tps();

        self.metrics = metrics;
        self.last_update = Instant::now();
        self.last_error = None;
    }

    pub fn update_rpc(&mut self, rpc_data: RpcData) {
        // Also update last block time from RPC if we have blocks
        if let Some(block) = rpc_data.recent_blocks.first() {
            if block.number > self.last_block_number {
                self.last_block_time = Some(Instant::now());
                self.last_block_number = block.number;
            }
        }

        self.rpc_data = rpc_data;
    }

    pub fn update_system(&mut self, system: SystemData) {
        self.system = system;
    }

    fn calculate_tps(&mut self) {
        if self.tx_samples.len() < 2 {
            return;
        }

        let oldest = self.tx_samples.front().unwrap();
        let newest = self.tx_samples.back().unwrap();

        let tx_delta = newest.tx_commits.saturating_sub(oldest.tx_commits);
        let time_delta_ms = newest.timestamp_ms.saturating_sub(oldest.timestamp_ms);

        if time_delta_ms > 0 {
            self.tps = (tx_delta as f64 / time_delta_ms as f64) * 1000.0;

            // Add to history for sparkline (capped at reasonable value for display)
            let tps_capped = (self.tps.min(10000.0)) as u64;
            self.tps_history.push_back(tps_capped);
            if self.tps_history.len() > TPS_HISTORY_SIZE {
                self.tps_history.pop_front();
            }
        }
    }

    pub fn set_error(&mut self, error: String) {
        self.last_error = Some(error);
    }

    pub fn time_since_last_block(&self) -> Option<Duration> {
        self.last_block_time.map(|t| t.elapsed())
    }

    pub fn block_height(&self) -> u64 {
        // Prefer RPC block number as it's more accurate
        if self.rpc_data.block_number > 0 {
            self.rpc_data.block_number
        } else {
            self.metrics.block_num
        }
    }

    pub fn recent_blocks(&self) -> &[Block] {
        &self.rpc_data.recent_blocks
    }

    pub fn tps_sparkline_data(&self) -> Vec<u64> {
        self.tps_history.iter().copied().collect()
    }

    pub fn sync_status(&self) -> &'static str {
        if self.metrics.is_synced() {
            "synced"
        } else {
            "syncing"
        }
    }

    pub fn peer_health(&self) -> &'static str {
        match self.metrics.peer_count {
            0 => "no peers",
            1..=10 => "low",
            11..=50 => "ok",
            _ => "healthy",
        }
    }
}
