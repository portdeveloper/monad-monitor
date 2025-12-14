use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::metrics::PrometheusMetrics;
use crate::rpc::{Block, RpcData};
use crate::system::SystemData;

const TPS_HISTORY_SIZE: usize = 300; // 5 minutes of history (fills wide terminals)
const SAMPLE_HISTORY_SIZE: usize = 10; // Keep last 10 samples for TPS calculation

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Theme {
    #[default]
    Gray,
    Light,
    Monad,      // Purple-heavy brand theme
    Matrix,     // Green on black hacker style
    Ocean,      // Blue tones
    Christmas,  // Festive red and green
}

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
    pub tps_peak: f64,
    tps_prev: f64,

    // Timing
    pub last_update: Instant,
    pub last_block_time: Option<Instant>,
    last_block_number: u64,

    // Latency tracking
    latency_prev: u64,
    peers_prev: u64,

    // Network rate tracking
    net_rx_prev: u64,
    net_tx_prev: u64,
    pub net_rx_rate: f64, // bytes per second
    pub net_tx_rate: f64,

    // Error tracking
    pub last_error: Option<String>,

    // UI theme
    pub theme: Theme,
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
            tps_peak: 0.0,
            tps_prev: 0.0,
            last_update: Instant::now(),
            last_block_time: None,
            last_block_number: 0,
            latency_prev: 0,
            peers_prev: 0,
            net_rx_prev: 0,
            net_tx_prev: 0,
            net_rx_rate: 0.0,
            net_tx_rate: 0.0,
            last_error: None,
            theme: Theme::Gray,
        }
    }

    pub fn toggle_theme(&mut self) {
        self.theme = match self.theme {
            Theme::Gray => Theme::Light,
            Theme::Light => Theme::Monad,
            Theme::Monad => Theme::Matrix,
            Theme::Matrix => Theme::Ocean,
            Theme::Ocean => Theme::Christmas,
            Theme::Christmas => Theme::Gray,
        };
    }

    pub fn theme_name(&self) -> &'static str {
        match self.theme {
            Theme::Gray => "gray",
            Theme::Light => "light",
            Theme::Monad => "monad",
            Theme::Matrix => "matrix",
            Theme::Ocean => "ocean",
            Theme::Christmas => "christmas",
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

        // Track latency and peers for trend
        self.latency_prev = self.metrics.latency_p99_ms;
        self.peers_prev = self.metrics.peer_count;

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
        // Calculate network rates (bytes per second)
        // System updates every 5 seconds
        const UPDATE_INTERVAL_SECS: f64 = 5.0;

        if self.net_rx_prev > 0 && system.net_rx_bytes > self.net_rx_prev {
            self.net_rx_rate = (system.net_rx_bytes - self.net_rx_prev) as f64 / UPDATE_INTERVAL_SECS;
        }
        if self.net_tx_prev > 0 && system.net_tx_bytes > self.net_tx_prev {
            self.net_tx_rate = (system.net_tx_bytes - self.net_tx_prev) as f64 / UPDATE_INTERVAL_SECS;
        }

        self.net_rx_prev = system.net_rx_bytes;
        self.net_tx_prev = system.net_tx_bytes;

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
            self.tps_prev = self.tps;
            self.tps = (tx_delta as f64 / time_delta_ms as f64) * 1000.0;

            // Track peak TPS
            if self.tps > self.tps_peak {
                self.tps_peak = self.tps;
            }

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

    /// Returns pulse intensity from 0.0 to 1.0 based on how recently a block arrived
    /// 1.0 = just now, fades to 0.0 over ~1 second
    pub fn pulse_intensity(&self) -> f64 {
        match self.last_block_time {
            Some(t) => {
                let elapsed_ms = t.elapsed().as_millis() as f64;
                let fade_duration_ms = 1000.0;
                (1.0 - (elapsed_ms / fade_duration_ms)).max(0.0)
            }
            None => 0.0,
        }
    }

    /// Returns TPS trend: 1 = up, -1 = down, 0 = stable
    pub fn tps_trend(&self) -> i8 {
        let threshold = 50.0; // Need 50 TPS difference to show trend
        if self.tps > self.tps_prev + threshold {
            1
        } else if self.tps < self.tps_prev - threshold {
            -1
        } else {
            0
        }
    }

    /// Returns latency trend: 1 = worsening, -1 = improving, 0 = stable
    pub fn latency_trend(&self) -> i8 {
        let current = self.metrics.latency_p99_ms;
        let threshold = 20; // Need 20ms difference to show trend
        if current > self.latency_prev + threshold {
            1 // Getting worse
        } else if current + threshold < self.latency_prev {
            -1 // Improving
        } else {
            0
        }
    }

    /// Returns peer count trend: 1 = up, -1 = down, 0 = stable
    pub fn peers_trend(&self) -> i8 {
        let current = self.metrics.peer_count;
        let threshold = 5; // Need 5 peer difference to show trend
        if current > self.peers_prev + threshold {
            1
        } else if current + threshold < self.peers_prev {
            -1
        } else {
            0
        }
    }

    /// Format bytes per second as human readable
    pub fn format_bandwidth(bytes_per_sec: f64) -> String {
        if bytes_per_sec >= 1_000_000_000.0 {
            format!("{:.1}GB/s", bytes_per_sec / 1_000_000_000.0)
        } else if bytes_per_sec >= 1_000_000.0 {
            format!("{:.1}MB/s", bytes_per_sec / 1_000_000.0)
        } else if bytes_per_sec >= 1_000.0 {
            format!("{:.0}KB/s", bytes_per_sec / 1_000.0)
        } else {
            format!("{:.0}B/s", bytes_per_sec)
        }
    }
}
