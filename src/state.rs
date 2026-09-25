use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::alerts::{AlertState, Sample};
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

    // Metrics polls in a row that brought no new TPS sample. Once a full
    // window's worth pass without one, the window describes the past, not the
    // present, and the rate decays to zero instead of replaying its last value.
    polls_since_sample: u32,

    // When the node's WebSocket last delivered a block, and whether the
    // subscription is up. Kept apart from `last_block_time`, which the metrics
    // poll also refreshes: a dead WebSocket has to stay visible even while
    // Prometheus keeps answering.
    last_rpc_block_at: Option<Instant>,
    pub ws_connected: bool,

    // Why the subscription is down, kept separate from `last_error` because a
    // successful metrics poll clears that one every second and would otherwise
    // wipe the reason a second after it appeared.
    pub ws_error: Option<String>,

    // Why the last webhook delivery failed, if it did. Cleared by the next
    // delivery that succeeds.
    pub webhook_error: Option<String>,

    // Whether each source has reported at least once. A threshold is not
    // evaluated against a value nobody has measured yet.
    metrics_seen: bool,
    system_seen: bool,

    // Which alert thresholds are currently tripped.
    pub alerts: AlertState,

    // Latency tracking
    latency_prev: Option<u64>,
    peers_prev: Option<u64>,

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
            polls_since_sample: 0,
            last_rpc_block_at: None,
            ws_connected: false,
            ws_error: None,
            webhook_error: None,
            metrics_seen: false,
            system_seen: false,
            alerts: AlertState::default(),
            latency_prev: None,
            peers_prev: None,
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
        // Track new block. An unread height is not a height that moved, so it
        // must not refresh the "last block seen" clock the staleness display
        // hangs off.
        if let Some(block_num) = metrics.block_num {
            if block_num > self.last_block_number {
                self.last_block_time = Some(Instant::now());
                self.last_block_number = block_num;
            }
        }

        // Add TX sample for TPS calculation
        let mut sampled = false;
        if let (Some(tx_commits), ts_ms @ 1..) =
            (metrics.tx_commits, metrics.tx_commits_timestamp_ms)
        {
            let sample = TxSample {
                tx_commits,
                timestamp_ms: ts_ms,
            };

            // Only add if timestamp is newer
            if self
                .tx_samples
                .back()
                .map(|s| sample.timestamp_ms > s.timestamp_ms)
                .unwrap_or(true)
            {
                // A cumulative counter only climbs. A reading below the one before it means
                // the source started over, not that transactions were undone, so nothing on
                // the far side of that restart can be subtracted from this reading: the
                // window is cleared and this sample becomes its first entry.
                //
                // Dropping the reading instead would leave the pre-restart samples in place
                // and hold the rate at zero until they aged out, which is the bug. Zeroing
                // the rate here would be the same mistake from the other side — a single
                // point is not a measurement. The next sample is the first one with a
                // baseline to be measured against, and the rate resumes there.
                if self
                    .tx_samples
                    .back()
                    .map(|s| sample.tx_commits < s.tx_commits)
                    .unwrap_or(false)
                {
                    self.tx_samples.clear();
                }

                self.tx_samples.push_back(sample);
                if self.tx_samples.len() > SAMPLE_HISTORY_SIZE {
                    self.tx_samples.pop_front();
                }
                sampled = true;
            }
        }

        if sampled {
            self.polls_since_sample = 0;
            self.calculate_tps();
        } else {
            // The counter's clock did not move, so recomputing from the same
            // window would only repeat the old rate. A short gap keeps the last
            // reading; a window's worth of empty polls means the source stopped
            // and the rate follows it down.
            self.polls_since_sample = self.polls_since_sample.saturating_add(1);
            if self.polls_since_sample as usize >= SAMPLE_HISTORY_SIZE {
                self.tps_prev = self.tps;
                self.tps = 0.0;
                self.tx_samples.clear();
            }
            self.push_tps_point();
        }

        // Track latency and peers for trend
        self.latency_prev = self.metrics.latency_p99_ms;
        self.peers_prev = self.metrics.peer_count;

        self.metrics = metrics;
        self.last_update = Instant::now();
        self.metrics_seen = true;
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

        if rpc_data.block_number > 0 {
            self.last_rpc_block_at = Some(Instant::now());
        }
        self.ws_connected = true;
        self.rpc_data = rpc_data;
    }

    /// The subscription came up.
    pub fn set_ws_connected(&mut self) {
        self.ws_connected = true;
        self.ws_error = None;
    }

    /// The subscription dropped. The reason stays on screen for as long as the
    /// stream is down rather than leaving the display frozen on stale blocks
    /// with nothing to explain it.
    pub fn set_ws_disconnected(&mut self, reason: String) {
        self.ws_connected = false;
        self.ws_error = Some(reason);
    }

    /// The result of one webhook delivery. A failure stays on screen until a
    /// later one gets through, so a webhook that has stopped working is not
    /// mistaken for a quiet night.
    pub fn set_webhook_result(&mut self, result: Result<(), String>) {
        self.webhook_error = result.err();
    }

    /// The readings the alert thresholds are evaluated against. A source that
    /// has not reported yet is left unknown so a fresh start cannot look like
    /// an incident.
    pub fn alert_sample(&self) -> Sample {
        Sample {
            secs_since_block: self.last_rpc_block_at.map(|t| t.elapsed().as_secs()),
            finalized_lag: self.system.finalized_lag(),
            peers: self.metrics.peer_count,
            disk_pct: self.system.disk_used_pct,
        }
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
        self.system_seen = true;
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

            self.push_tps_point();
        }
    }

    // Add to history for sparkline (capped at reasonable value for display)
    fn push_tps_point(&mut self) {
        let tps_capped = (self.tps.min(10000.0)) as u64;
        self.tps_history.push_back(tps_capped);
        if self.tps_history.len() > TPS_HISTORY_SIZE {
            self.tps_history.pop_front();
        }
    }

    pub fn set_error(&mut self, error: String) {
        self.last_error = Some(error);
    }

    pub fn time_since_last_block(&self) -> Option<Duration> {
        self.last_block_time.map(|t| t.elapsed())
    }

    /// The height to show, or `None` when neither source has reported one.
    ///
    /// Inventing a zero here would undo the point of the metric being optional:
    /// zero is a real height a node at genesis reports, and the snapshot would
    /// say `block_num: null` next to `block_height: 0`.
    pub fn block_height(&self) -> Option<u64> {
        // 0 is this field's own "not reported" marker on the RPC side.
        let rpc = (self.rpc_data.block_number > 0).then_some(self.rpc_data.block_number);

        // The WebSocket's height leads while the subscription is up. Once it
        // drops, that number only ages, and the metrics poll is still
        // reporting; taking the higher of the two keeps the header moving
        // instead of frozen at the moment the stream died.
        if !self.ws_connected {
            return match (rpc, self.metrics.block_num) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (found, None) | (None, found) => found,
            };
        }
        // Prefer RPC block number as it's more accurate
        rpc.or(self.metrics.block_num)
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
            // A scrape that never carried the metric is not a node with no
            // peers. Say so, rather than colouring an unmeasured field red.
            None => "...",
            Some(0) => "no peers",
            Some(1..=10) => "low",
            Some(11..=50) => "ok",
            Some(_) => "healthy",
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
        // Two real readings or no arrow, the same rule peers_trend follows: an
        // unknown treated as zero draws an improvement on the first reading and
        // a worsening the moment the metric comes back.
        match (self.metrics.latency_p99_ms, self.latency_prev) {
            (Some(current), Some(prev)) => {
                let threshold = 20; // Need 20ms difference to show trend
                if current > prev + threshold {
                    1 // Getting worse
                } else if current + threshold < prev {
                    -1 // Improving
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Returns peer count trend: 1 = up, -1 = down, 0 = stable
    pub fn peers_trend(&self) -> i8 {
        // A trend needs two readings. Treating an unknown as zero drew a
        // rising arrow on the first reading, and a falling one the moment the
        // metric dropped out -- neither of which was a move.
        match (self.metrics.peer_count, self.peers_prev) {
            (Some(current), Some(prev)) => {
                let threshold = 5; // Need 5 peer difference to show trend
                if current > prev + threshold {
                    1
                } else if current + threshold < prev {
                    -1
                } else {
                    0
                }
            }
            _ => 0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::PrometheusMetrics;
    use crate::rpc::RpcData;

    fn metrics_at(commits: u64, ts_ms: u64) -> PrometheusMetrics {
        PrometheusMetrics {
            tx_commits: Some(commits),
            tx_commits_timestamp_ms: ts_ms,
            ..Default::default()
        }
    }

    #[test]
    fn a_brief_gap_keeps_the_last_rate() {
        let mut state = AppState::new();
        state.update_metrics(metrics_at(0, 1_000));
        state.update_metrics(metrics_at(500, 2_000));
        let live = state.tps;
        assert!(live > 0.0);

        state.update_metrics(metrics_at(500, 2_000));
        assert_eq!(state.tps, live);
    }

    #[test]
    fn tps_decays_to_zero_when_the_counter_stops() {
        let mut state = AppState::new();
        state.update_metrics(metrics_at(0, 1_000));
        state.update_metrics(metrics_at(500, 2_000));
        state.update_metrics(metrics_at(1_000, 3_000));
        assert!(state.tps > 0.0);

        // The same frozen reading for a full window's worth of polls.
        for _ in 0..SAMPLE_HISTORY_SIZE {
            state.update_metrics(metrics_at(1_000, 3_000));
        }
        assert_eq!(state.tps, 0.0);
        assert_eq!(state.tps_sparkline_data().last(), Some(&0));
    }

    #[test]
    fn tps_comes_back_when_the_counter_resumes() {
        let mut state = AppState::new();
        state.update_metrics(metrics_at(0, 1_000));
        state.update_metrics(metrics_at(500, 2_000));
        for _ in 0..SAMPLE_HISTORY_SIZE {
            state.update_metrics(metrics_at(500, 2_000));
        }
        assert_eq!(state.tps, 0.0);

        state.update_metrics(metrics_at(600, 60_000));
        state.update_metrics(metrics_at(1_100, 61_000));
        assert!((state.tps - 500.0).abs() < 1.0);
    }

    #[test]
    fn a_restarted_counter_starts_a_fresh_window() {
        // The sequence from the bug report. A node restart resets the cumulative counter
        // while its clock keeps running, so the pre-restart samples cannot be subtracted
        // from the ones after it.
        let mut state = AppState::new();

        state.update_metrics(metrics_at(1_000, 1_000));
        assert_eq!(state.tps, 0.0, "one sample is not a rate");

        state.update_metrics(metrics_at(1_010, 2_000));
        assert!(
            (state.tps - 10.0).abs() < 1e-9,
            "before the restart: {}",
            state.tps
        );

        // The restart itself. The reading is kept as the new baseline, and the rate stays
        // where it was rather than being invented from a single point.
        state.update_metrics(metrics_at(1, 3_000));
        assert!(
            (state.tps - 10.0).abs() < 1e-9,
            "at the restart: {}",
            state.tps
        );

        // Ten transactions in the second after the restart.
        state.update_metrics(metrics_at(11, 4_000));
        assert!(
            (state.tps - 10.0).abs() < 1e-9,
            "after the restart: {}",
            state.tps
        );

        // One more poll, deliberately at a different rate. The reported sequence runs at ten
        // either side of the restart, so a window that was merely left empty would hold the
        // old reading and pass for the right answer. Thirty transactions in this second put
        // the two-second window at twenty, which the rate before the restart cannot be
        // mistaken for.
        state.update_metrics(metrics_at(41, 5_000));
        assert!(
            (state.tps - 20.0).abs() < 1e-9,
            "the window after the restart is measured, not held: {}",
            state.tps
        );
    }

    #[test]
    fn a_counter_that_restarts_at_zero_is_no_different() {
        let mut state = AppState::new();
        state.update_metrics(metrics_at(1_000, 1_000));
        state.update_metrics(metrics_at(1_010, 2_000));

        state.update_metrics(metrics_at(0, 3_000));
        state.update_metrics(metrics_at(10, 4_000));
        assert!(
            (state.tps - 10.0).abs() < 1e-9,
            "after a restart at zero: {}",
            state.tps
        );
    }

    #[test]
    fn a_restart_does_not_spike_the_rate_or_the_peak() {
        // The other way to get this wrong: measure the post-restart reading against the
        // pre-restart baseline and report the whole counter as one second's work.
        let mut state = AppState::new();
        state.update_metrics(metrics_at(1_000_000, 1_000));
        state.update_metrics(metrics_at(1_000_010, 2_000));
        let before = state.tps_peak;

        state.update_metrics(metrics_at(5, 3_000));
        state.update_metrics(metrics_at(15, 4_000));

        assert!(
            (state.tps - 10.0).abs() < 1e-9,
            "rate after the restart: {}",
            state.tps
        );
        assert_eq!(state.tps_peak, before, "the restart moved the peak");
        assert!(
            state.tps_sparkline_data().iter().all(|&p| p <= 10),
            "a spike reached the sparkline: {:?}",
            state.tps_sparkline_data()
        );
    }

    #[test]
    fn a_reading_the_clock_rejects_leaves_the_window_alone() {
        // A lower counter only means a restart when the sample is one the window would
        // otherwise accept. A stale frame that arrives with an old timestamp is rejected
        // first, exactly as before, and must not clear anything on its way out.
        let mut state = AppState::new();
        state.update_metrics(metrics_at(1_000, 1_000));
        state.update_metrics(metrics_at(1_010, 2_000));

        state.update_metrics(metrics_at(5, 1_500));

        state.update_metrics(metrics_at(1_030, 3_000));
        assert!(
            (state.tps - 15.0).abs() < 1e-9,
            "the window was not the one that survived: {}",
            state.tps
        );
    }

    #[test]
    fn a_counter_that_stands_still_keeps_its_window() {
        // Equal is not a restart. An idle node reports the same total against a moving
        // clock, and that reading belongs in the window: it is how a quiet minute shows up
        // as a falling rate instead of a frozen one.
        let mut state = AppState::new();
        state.update_metrics(metrics_at(1_000, 1_000));
        state.update_metrics(metrics_at(1_010, 2_000));
        assert!((state.tps - 10.0).abs() < 1e-9);

        state.update_metrics(metrics_at(1_010, 3_000));
        assert!(
            (state.tps - 5.0).abs() < 1e-9,
            "ten transactions over two seconds: {}",
            state.tps
        );
    }

    #[test]
    fn a_restart_above_the_oldest_sample_is_still_a_restart() {
        // The comparison is against the newest sample, not the oldest one. A node that
        // comes back and climbs past the front of the window is still a node that started
        // over, and measuring the difference from the front invents transactions.
        let mut state = AppState::new();
        state.update_metrics(metrics_at(1_000, 1_000));
        state.update_metrics(metrics_at(2_000, 2_000));
        let before = state.tps;
        assert!((before - 1_000.0).abs() < 1e-9);

        state.update_metrics(metrics_at(1_500, 3_000));
        assert!(
            (state.tps - before).abs() < 1e-9,
            "the restart was measured instead of being recognised: {}",
            state.tps
        );

        state.update_metrics(metrics_at(1_600, 4_000));
        assert!(
            (state.tps - 100.0).abs() < 1e-9,
            "a hundred transactions in the second after the restart: {}",
            state.tps
        );
    }

    #[test]
    fn the_rate_still_decays_after_a_restart() {
        // The fresh window is a window like any other: when the counter stops moving
        // inside it, the rate comes down the same way.
        let mut state = AppState::new();
        state.update_metrics(metrics_at(1_000, 1_000));
        state.update_metrics(metrics_at(1_010, 2_000));
        state.update_metrics(metrics_at(1, 3_000));
        state.update_metrics(metrics_at(11, 4_000));
        assert!(state.tps > 0.0);

        for _ in 0..SAMPLE_HISTORY_SIZE {
            state.update_metrics(metrics_at(11, 4_000));
        }
        assert_eq!(state.tps, 0.0);
    }

    #[test]
    fn an_unread_disk_and_lag_reach_the_thresholds_as_unknown() {
        // Every source inside SystemClient::fetch failed, so fetch returns a
        // default SystemData and update_system stores it. The alert engine has
        // to be told these were never measured, not that they read zero.
        let mut state = AppState::new();
        state.update_system(SystemData::default());

        let sample = state.alert_sample();
        assert_eq!(sample.disk_pct, None);
        assert_eq!(sample.finalized_lag, None);
    }

    #[test]
    fn a_real_reading_still_reaches_the_thresholds() {
        let mut state = AppState::new();
        state.update_system(SystemData {
            disk_used_pct: Some(91.0),
            history_latest: Some(500),
            latest_finalized: Some(488),
            ..Default::default()
        });

        let sample = state.alert_sample();
        assert_eq!(sample.disk_pct, Some(91.0));
        assert_eq!(sample.finalized_lag, Some(12));
    }

    #[test]
    fn an_unread_peer_count_reaches_the_threshold_as_unknown() {
        // The scrape succeeded and carried a block height, but no peer metric.
        // The alert engine has to be told it was never measured, otherwise the
        // low-peer threshold trips on a reading nobody took.
        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            block_num: Some(100),
            ..Default::default()
        });

        assert_eq!(state.alert_sample().peers, None);
        assert_eq!(state.peer_health(), "...");
    }

    #[test]
    fn a_real_zero_peer_count_still_reaches_the_threshold() {
        // Zero peers is a measurement, and it is the one the alert exists for.
        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            peer_count: Some(0),
            ..Default::default()
        });

        assert_eq!(state.alert_sample().peers, Some(0));
        assert_eq!(state.peer_health(), "no peers");
    }

    #[test]
    fn a_real_move_between_two_readings_sets_the_trend() {
        // The unknown cases above say nothing about the comparison itself, so
        // this binds it: a rise, a fall, a move inside the threshold, and the
        // boundary that separates the first two from the third.
        let mut state = AppState::new();
        let reading = |n| PrometheusMetrics {
            peer_count: Some(n),
            ..Default::default()
        };

        state.update_metrics(reading(10));
        state.update_metrics(reading(20));
        assert_eq!(state.peers_trend(), 1, "10 -> 20 is a rise");

        state.update_metrics(reading(5));
        assert_eq!(state.peers_trend(), -1, "20 -> 5 is a fall");

        state.update_metrics(reading(7));
        assert_eq!(state.peers_trend(), 0, "5 -> 7 is inside the threshold");

        state.update_metrics(reading(12));
        assert_eq!(state.peers_trend(), 0, "a gain of exactly 5 is not a move");
    }

    #[test]
    fn an_unread_height_is_unknown_rather_than_genesis() {
        // The snapshot must not say block_num: null next to block_height: 0.
        // Zero is a height a node at genesis genuinely reports, so inventing it
        // here would undo exactly what making the field optional bought.
        // Both branches: the websocket leads while it is up, and the metrics
        // poll takes over once it drops. Neither may invent a height.
        let mut state = AppState::new();
        assert_eq!(state.metrics.block_num, None);
        assert_eq!(
            state.block_height(),
            None,
            "an unread height read as genesis while disconnected"
        );

        state.set_ws_connected();
        assert_eq!(
            state.block_height(),
            None,
            "an unread height read as genesis while connected"
        );
        state.set_ws_disconnected("gone".to_string());

        // And with no local height there is nothing to compare against.
        assert_eq!(
            state.system.block_difference(state.block_height()),
            None,
            "a difference was computed against a height nobody read"
        );

        // A real zero still reads as a height.
        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            block_num: Some(0),
            ..Default::default()
        });
        assert_eq!(
            state.block_height(),
            Some(0),
            "a node at genesis lost its height"
        );
    }

    #[test]
    fn an_unread_height_does_not_refresh_the_block_clock() {
        // last_block_time is what the display leans on to say a node has gone
        // quiet. A scrape that carried no readable height has not seen a block,
        // so it must not reset that clock -- otherwise a wedged exporter looks
        // like a node that is still producing.
        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            block_num: Some(4200),
            ..Default::default()
        });
        let seen_at = state.last_block_time;
        assert_eq!(state.last_block_number, 4200);
        assert!(seen_at.is_some());

        state.update_metrics(PrometheusMetrics {
            block_num: None,
            ..Default::default()
        });

        assert_eq!(
            state.last_block_number, 4200,
            "an unread height moved the height"
        );
        assert_eq!(
            state.last_block_time, seen_at,
            "an unread height refreshed the last-block clock"
        );
    }

    #[test]
    fn a_height_that_did_not_advance_leaves_the_block_clock_alone() {
        // The clock marks when a NEW block was seen. A scrape that repeats the
        // height, or reports an older one, has not seen one -- and a repeated
        // height is the normal case between blocks, so getting this wrong would
        // make every poll look like a block.
        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            block_num: Some(4200),
            ..Default::default()
        });
        let seen_at = state.last_block_time;

        for height in [4200, 4199, 0] {
            state.update_metrics(PrometheusMetrics {
                block_num: Some(height),
                ..Default::default()
            });
            assert_eq!(state.last_block_number, 4200, "{} moved the height", height);
            assert_eq!(
                state.last_block_time, seen_at,
                "{} refreshed the clock",
                height
            );
        }

        state.update_metrics(PrometheusMetrics {
            block_num: Some(4201),
            ..Default::default()
        });
        assert_eq!(state.last_block_number, 4201);
        assert_ne!(
            state.last_block_time, seen_at,
            "a new block did not move the clock"
        );
    }

    #[test]
    fn a_real_move_between_two_latency_readings_sets_the_trend() {
        // Both comparisons, both boundaries. The threshold is 20ms and the test
        // pins that exactly 20 is NOT a move, so turning either `>` into `>=`
        // fails here rather than passing quietly.
        for (prev, current, expected) in [
            (100u64, 121u64, 1i8), // a rise past the threshold
            (100, 120, 0),         // exactly the threshold is not a move
            (100, 110, 0),         // inside the threshold
            (121, 100, -1),        // a fall past the threshold
            (120, 100, 0),         // exactly the threshold, the other way
            (100, 100, 0),         // no move at all
        ] {
            let mut state = AppState::new();
            state.update_metrics(PrometheusMetrics {
                latency_p99_ms: Some(prev),
                ..Default::default()
            });
            state.update_metrics(PrometheusMetrics {
                latency_p99_ms: Some(current),
                ..Default::default()
            });

            assert_eq!(
                state.latency_trend(),
                expected,
                "{}ms -> {}ms",
                prev,
                current
            );
        }
    }

    #[test]
    fn an_unknown_latency_draws_no_trend() {
        // Same rule as the peer trend: two real readings or no arrow. Treating
        // an unknown as zero would draw an improvement on the first reading and
        // a worsening the moment the metric came back.
        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            latency_p99_ms: Some(400),
            ..Default::default()
        });
        assert_eq!(state.latency_trend(), 0, "no previous reading to compare");

        state.update_metrics(PrometheusMetrics {
            latency_p99_ms: None,
            ..Default::default()
        });
        assert_eq!(state.latency_trend(), 0, "current reading is unknown");

        state.update_metrics(PrometheusMetrics {
            latency_p99_ms: Some(40),
            ..Default::default()
        });
        assert_eq!(state.latency_trend(), 0, "the previous reading was unknown");

        state.update_metrics(PrometheusMetrics {
            latency_p99_ms: Some(400),
            ..Default::default()
        });
        assert_eq!(state.latency_trend(), 1, "a real move between two readings");
    }

    #[test]
    fn an_unknown_peer_count_draws_no_trend() {
        // peers_prev starts unknown, so the first real reading has nothing to
        // compare against; treating unknown as 0 would draw a rising arrow on
        // the first scrape and a falling one when the metric dropped out.
        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            peer_count: Some(40),
            ..Default::default()
        });
        assert_eq!(state.peers_trend(), 0, "no previous reading to compare");

        state.update_metrics(PrometheusMetrics {
            block_num: Some(100),
            ..Default::default()
        });
        assert_eq!(state.peers_trend(), 0, "current reading is unknown");
    }

    #[test]
    fn the_largest_countable_peer_reading_does_not_overflow_the_trend() {
        // peers_trend adds 5 to the previous reading. A `+Inf` on the metric line
        // used to saturate the cast to u64::MAX, and `prev + 5` then overflowed:
        // a panic in debug, a wrap in release. The parser now refuses anything
        // from 2^64 up, so the largest value that can reach here is the biggest
        // integer an f64 holds below that -- and it has to stay inside u64.
        const LARGEST: u64 = 18_446_744_073_709_549_568; // 2^64 - 2048

        let mut state = AppState::new();
        state.update_metrics(PrometheusMetrics {
            peer_count: Some(LARGEST),
            ..Default::default()
        });
        state.update_metrics(PrometheusMetrics {
            peer_count: Some(LARGEST),
            ..Default::default()
        });

        assert_eq!(state.peers_trend(), 0);
        assert_eq!(LARGEST.checked_add(5), Some(18_446_744_073_709_549_573));
    }

    #[test]
    fn the_height_follows_metrics_once_the_websocket_drops() {
        let mut state = AppState::new();
        state.update_rpc(RpcData {
            block_number: 90,
            ..Default::default()
        });
        state.update_metrics(PrometheusMetrics {
            block_num: Some(100),
            ..Default::default()
        });
        // While the subscription is up its height leads.
        assert_eq!(state.block_height(), Some(90));

        state.set_ws_disconnected("gone".to_string());
        assert_eq!(state.block_height(), Some(100));

        state.set_ws_connected();
        assert_eq!(state.block_height(), Some(90));
    }
}
