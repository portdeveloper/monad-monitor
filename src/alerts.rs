//! Alert thresholds and webhook notification.
//!
//! Thresholds are evaluated against values the monitor already tracks. A
//! threshold fires on the ok -> alert flip and again on alert -> ok, never on
//! the refresh ticks in between, so an incident is two webhook messages instead
//! of a stream. Two things keep a noisy metric from filling a channel: a flip
//! is only accepted once the new side has held for `confirm_samples`
//! consecutive evaluations, which absorbs brief crossings, and a threshold that
//! has already alerted stays quiet for `cooldown_secs`, which absorbs a value
//! that genuinely drifts back and forth across its threshold for minutes at a
//! time. A recovery is never held back, so an incident that was announced is
//! always closed.
//!
//! Nothing here writes to the node: a tripped threshold reads state and posts a
//! notification, which keeps the monitor in its read-only lane.

use std::time::Duration;

use serde_json::{json, Value};

/// How long a webhook POST is allowed to take. A slow endpoint must never hold
/// up the draw loop, so the request is spawned and this is its whole budget.
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// What a threshold watches. Each kind carries its own confirm counter and its
/// own tripped flag, so one noisy metric cannot mask another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    NoBlock,
    FinalizedLag,
    LowPeers,
    DiskFull,
}

impl AlertKind {
    /// Stable machine-readable name, used as the `alert` field in the payload.
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertKind::NoBlock => "no_block",
            AlertKind::FinalizedLag => "finalized_lag",
            AlertKind::LowPeers => "low_peers",
            AlertKind::DiskFull => "disk_full",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            AlertKind::NoBlock => "no new block",
            AlertKind::FinalizedLag => "finalized lag",
            AlertKind::LowPeers => "peer count",
            AlertKind::DiskFull => "disk usage",
        }
    }
}

/// Thresholds and the webhook to notify. Every threshold is optional: one that
/// is not set is never evaluated, so running with no configuration behaves
/// exactly as before.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertConfig {
    pub webhook_url: Option<String>,
    /// Seconds without a new block from the node's WebSocket stream.
    pub no_block_secs: Option<u64>,
    /// Blocks between the latest known block and the latest finalized one.
    pub finalized_lag: Option<u64>,
    /// Alert when the peer count drops below this.
    pub min_peers: Option<u64>,
    /// Alert when disk usage rises above this percentage.
    pub disk_pct: Option<f64>,
    /// Consecutive evaluations a change has to survive before it flips.
    pub confirm_samples: u32,
    /// Shortest gap between two alerts for the same threshold. The confirm
    /// window only absorbs brief crossings; a metric that genuinely sits near
    /// its threshold and drifts across it for minutes at a time would still
    /// send a pair every time, which is the spam this prevents.
    pub cooldown_secs: u64,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            no_block_secs: None,
            finalized_lag: None,
            min_peers: None,
            disk_pct: None,
            confirm_samples: 3,
            cooldown_secs: 300,
        }
    }
}

impl AlertConfig {
    /// True when at least one threshold is set. Used to skip the whole path
    /// when the monitor runs unconfigured.
    pub fn is_enabled(&self) -> bool {
        self.no_block_secs.is_some()
            || self.finalized_lag.is_some()
            || self.min_peers.is_some()
            || self.disk_pct.is_some()
    }

    /// Whether at least one threshold or a webhook was configured. Used to
    /// keep the alert flags out of the headless snapshot mode, which has no
    /// place to raise or clear an alert.
    pub fn is_configured(&self) -> bool {
        self.is_enabled() || self.webhook_url.is_some()
    }

    /// Fills one alert flag in. The caller owns the argument loop, so this
    /// stays a single-flag operation rather than a second parser: an unknown
    /// flag is the caller's error to report, which is what makes a typo like
    /// `--alert-noblock` fail loudly instead of silently never arming.
    pub fn apply(&mut self, flag: &str, value: &str) -> Result<(), String> {
        match flag {
            "--webhook-url" => {
                // Delivery failures are deliberately quiet, so a typo here
                // would otherwise look exactly like a working webhook that
                // nobody reads. Catch the obvious case up front.
                if !value.starts_with("http://") && !value.starts_with("https://") {
                    return Err(format!(
                        "--webhook-url must start with http:// or https://, got {:?}",
                        value
                    ));
                }
                self.webhook_url = Some(value.to_string());
            }
            "--alert-no-block" => self.no_block_secs = Some(parse_num(flag, value)?),
            "--alert-finalized-lag" => self.finalized_lag = Some(parse_num(flag, value)?),
            "--alert-min-peers" => self.min_peers = Some(parse_num(flag, value)?),
            "--alert-disk" => self.disk_pct = Some(parse_num(flag, value)?),
            "--alert-confirm" => {
                let samples: u32 = parse_num(flag, value)?;
                if samples == 0 {
                    return Err("--alert-confirm must be at least 1".to_string());
                }
                self.confirm_samples = samples;
            }
            "--alert-cooldown" => self.cooldown_secs = parse_num(flag, value)?,
            _ => return Err(format!("not an alert flag: {}", flag)),
        }
        Ok(())
    }
}

/// Whether the argument loop should hand this flag to [`AlertConfig::apply`].
/// Every alert flag takes a value.
pub fn is_alert_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--webhook-url"
            | "--alert-no-block"
            | "--alert-finalized-lag"
            | "--alert-min-peers"
            | "--alert-disk"
            | "--alert-confirm"
            | "--alert-cooldown"
    )
}

fn parse_num<T>(flag: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .parse::<T>()
        .map_err(|_| format!("{} expects a number, got {:?}", flag, value))
}

/// One evaluation's worth of readings. A field is `None` while its source has
/// not reported yet, and an unknown reading never trips or clears a threshold:
/// a monitor that just started has not observed a problem, it has observed
/// nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub secs_since_block: Option<u64>,
    pub finalized_lag: Option<u64>,
    pub peers: Option<u64>,
    pub disk_pct: Option<f64>,
}

/// What one notification would be, once the entry decides to send it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Notify {
    Firing,
    Resolved,
}

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    tripped: bool,
    /// Consecutive evaluations that disagree with `tripped`.
    streak: u32,
    /// Whether the current alert was announced. A recovery is only worth
    /// sending for an alert somebody heard about.
    announced: bool,
    /// When the last alert went out, so a threshold the metric keeps drifting
    /// across cannot send a pair every time it does.
    last_fired_at: Option<u64>,
}

impl Entry {
    /// Feeds one reading in and reports the notification it warrants, if any.
    fn observe(&mut self, breached: bool, confirm: u32, cooldown: u64, now: u64) -> Option<Notify> {
        if breached == self.tripped {
            self.streak = 0;
            return None;
        }

        self.streak += 1;
        if self.streak < confirm.max(1) {
            return None;
        }

        self.tripped = breached;
        self.streak = 0;

        if breached {
            // A repeat inside the cooldown still trips the cell on screen, it
            // just does not send. Staying silent here is what keeps a metric
            // hovering at its threshold from filling a channel.
            let quiet = self
                .last_fired_at
                .is_some_and(|last| now.saturating_sub(last) < cooldown);
            if quiet {
                self.announced = false;
                return None;
            }

            self.last_fired_at = Some(now);
            self.announced = true;
            Some(Notify::Firing)
        } else if self.announced {
            // Recovery is never held back: it closes an incident that was
            // already reported, and delaying it would leave a stale alert
            // standing.
            self.announced = false;
            Some(Notify::Resolved)
        } else {
            None
        }
    }
}

/// Which thresholds are currently tripped. Lives in `AppState` so the UI can
/// colour a cell without knowing anything about webhooks.
#[derive(Debug, Clone, Default)]
pub struct AlertState {
    no_block: Entry,
    finalized_lag: Entry,
    low_peers: Entry,
    disk_full: Entry,
}

impl AlertState {
    pub fn is_tripped(&self, kind: AlertKind) -> bool {
        match kind {
            AlertKind::NoBlock => self.no_block.tripped,
            AlertKind::FinalizedLag => self.finalized_lag.tripped,
            AlertKind::LowPeers => self.low_peers.tripped,
            AlertKind::DiskFull => self.disk_full.tripped,
        }
    }

    fn entry(&mut self, kind: AlertKind) -> &mut Entry {
        match kind {
            AlertKind::NoBlock => &mut self.no_block,
            AlertKind::FinalizedLag => &mut self.finalized_lag,
            AlertKind::LowPeers => &mut self.low_peers,
            AlertKind::DiskFull => &mut self.disk_full,
        }
    }
}

/// A threshold changing state. One of these becomes exactly one webhook
/// message.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub kind: AlertKind,
    /// True for ok -> alert, false for alert -> ok.
    pub firing: bool,
    /// The reading that caused the flip, already formatted for a human.
    pub value: String,
    /// The configured threshold, for context in the message.
    pub threshold: String,
}

impl Transition {
    /// The human-readable line. Discord and Slack both render this directly;
    /// a generic consumer can ignore it and read the structured fields.
    pub fn message(&self, node: &str) -> String {
        let node = if node.is_empty() { "monad node" } else { node };
        if self.firing {
            format!(
                "\u{1F534} {}: {} ({}, threshold {})",
                node,
                self.kind.label(),
                self.value,
                self.threshold
            )
        } else {
            format!(
                "\u{2705} {}: {} recovered ({})",
                node,
                self.kind.label(),
                self.value
            )
        }
    }

    /// The POST body.
    ///
    /// `content` is what Discord renders and `text` is what Slack renders, so
    /// one payload works against either webhook endpoint unchanged; both
    /// services ignore the fields they do not know, which leaves the structured
    /// fields for anything else consuming the hook.
    pub fn payload(&self, node: &str) -> Value {
        let message = self.message(node);
        json!({
            "content": message,
            "text": message,
            "alert": self.kind.as_str(),
            "status": if self.firing { "firing" } else { "resolved" },
            "value": self.value,
            "threshold": self.threshold,
            "node": node,
        })
    }
}

/// Evaluates every configured threshold against one sample and returns the
/// notifications it warrants. An empty result is the normal case: either
/// nothing changed, or the change was one the cooldown absorbed.
///
/// `now` is a monotonic count of seconds; only differences matter.
pub fn evaluate(
    state: &mut AlertState,
    config: &AlertConfig,
    sample: &Sample,
    now: u64,
) -> Vec<Transition> {
    let confirm = config.confirm_samples;
    let cooldown = config.cooldown_secs;
    let mut transitions = Vec::new();

    let mut check = |kind: AlertKind, breached: bool, value: String, threshold: String| {
        if let Some(notify) = state.entry(kind).observe(breached, confirm, cooldown, now) {
            transitions.push(Transition {
                kind,
                firing: notify == Notify::Firing,
                value,
                threshold,
            });
        }
    };

    if let (Some(limit), Some(secs)) = (config.no_block_secs, sample.secs_since_block) {
        check(
            AlertKind::NoBlock,
            secs >= limit,
            format!("{}s since the last block", secs),
            format!("{}s", limit),
        );
    }

    if let (Some(limit), Some(lag)) = (config.finalized_lag, sample.finalized_lag) {
        check(
            AlertKind::FinalizedLag,
            lag > limit,
            format!("{} blocks", lag),
            format!("{} blocks", limit),
        );
    }

    if let (Some(limit), Some(peers)) = (config.min_peers, sample.peers) {
        check(
            AlertKind::LowPeers,
            peers < limit,
            format!("{} peers", peers),
            format!("{} peers", limit),
        );
    }

    if let (Some(limit), Some(pct)) = (config.disk_pct, sample.disk_pct) {
        check(
            AlertKind::DiskFull,
            pct > limit,
            format!("{:.1}%", pct),
            format!("{:.0}%", limit),
        );
    }

    transitions
}

/// Posts one transition.
///
/// A failure is reported rather than swallowed: an alerting path that quietly
/// stops delivering is worse than no alerting at all, because the screen still
/// looks like everything is covered.
pub async fn post(client: reqwest::Client, url: String, body: Value) -> Result<(), String> {
    let response = client
        .post(&url)
        .json(&body)
        .timeout(WEBHOOK_TIMEOUT)
        .send()
        .await
        // Without `without_url` the error carries the request URL, and a
        // Discord or Slack webhook keeps its auth token in the path: a refused
        // connection would print the secret straight into the footer.
        .map_err(|e| e.without_url().to_string())?;

    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        // Discord and Slack both explain a rejected payload in the body, and
        // that explanation is the whole value of the message.
        let body = response.text().await.unwrap_or_default();
        let body: String = body.chars().take(200).collect();
        Err(format!("{} {}", status.as_u16(), body.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AlertConfig {
        AlertConfig {
            webhook_url: Some("http://localhost/hook".to_string()),
            no_block_secs: Some(30),
            finalized_lag: Some(10),
            min_peers: Some(5),
            disk_pct: Some(80.0),
            confirm_samples: 2,
            cooldown_secs: 0,
        }
    }

    fn peers(n: u64) -> Sample {
        Sample {
            peers: Some(n),
            ..Sample::default()
        }
    }

    #[test]
    fn fires_once_after_the_confirm_window_and_stays_quiet() {
        let config = config();
        let mut state = AlertState::default();

        // First breach is held back by the confirm window.
        assert!(evaluate(&mut state, &config, &peers(1), 0).is_empty());
        assert!(!state.is_tripped(AlertKind::LowPeers));

        let fired = evaluate(&mut state, &config, &peers(1), 0);
        assert_eq!(fired.len(), 1);
        assert!(fired[0].firing);
        assert_eq!(fired[0].kind, AlertKind::LowPeers);
        assert!(state.is_tripped(AlertKind::LowPeers));

        // Still breached: no repeats, however long it lasts.
        for _ in 0..10 {
            assert!(evaluate(&mut state, &config, &peers(1), 0).is_empty());
        }
    }

    #[test]
    fn recovers_once() {
        let config = config();
        let mut state = AlertState::default();
        evaluate(&mut state, &config, &peers(1), 0);
        evaluate(&mut state, &config, &peers(1), 0);
        assert!(state.is_tripped(AlertKind::LowPeers));

        assert!(evaluate(&mut state, &config, &peers(50), 0).is_empty());
        let recovered = evaluate(&mut state, &config, &peers(50), 0);
        assert_eq!(recovered.len(), 1);
        assert!(!recovered[0].firing);
        assert!(!state.is_tripped(AlertKind::LowPeers));

        for _ in 0..10 {
            assert!(evaluate(&mut state, &config, &peers(50), 0).is_empty());
        }
    }

    #[test]
    fn a_value_sitting_on_the_threshold_never_flips() {
        let config = config();
        let mut state = AlertState::default();

        // Alternating either side of the line, never long enough to confirm.
        for _ in 0..20 {
            assert!(evaluate(&mut state, &config, &peers(4), 0).is_empty());
            assert!(evaluate(&mut state, &config, &peers(6), 0).is_empty());
        }
        assert!(!state.is_tripped(AlertKind::LowPeers));
    }

    #[test]
    fn a_metric_drifting_across_the_threshold_sends_one_pair_per_cooldown() {
        let config = AlertConfig {
            min_peers: Some(5),
            confirm_samples: 1,
            cooldown_secs: 300,
            ..AlertConfig::default()
        };
        let mut state = AlertState::default();

        // Twenty minutes of a value crossing the line every thirty seconds.
        // The confirm window cannot help here: each side genuinely holds long
        // enough to be real, which is exactly the shape that fills a channel.
        let mut sent = 0;
        for round in 0..40u64 {
            let sample = peers(if round % 2 == 0 { 1 } else { 50 });
            sent += evaluate(&mut state, &config, &sample, round * 30).len();
        }

        // Four alerts and their four recoveries, one pair per cooldown, rather
        // than one per crossing.
        assert_eq!(sent, 8);
    }

    #[test]
    fn a_recovery_is_never_held_back_by_the_cooldown() {
        let config = AlertConfig {
            no_block_secs: Some(30),
            confirm_samples: 1,
            cooldown_secs: 300,
            ..AlertConfig::default()
        };
        let mut state = AlertState::default();

        let flowing = Sample {
            secs_since_block: Some(1),
            ..Sample::default()
        };
        let stalled = Sample {
            secs_since_block: Some(31),
            ..Sample::default()
        };

        assert!(evaluate(&mut state, &config, &flowing, 0).is_empty());

        let fired = evaluate(&mut state, &config, &stalled, 10);
        assert_eq!(fired.len(), 1);
        assert!(fired[0].firing);

        // Well inside the cooldown, but this closes the incident that was
        // announced, so holding it would leave a stale alert standing.
        let recovered = evaluate(&mut state, &config, &flowing, 40);
        assert_eq!(recovered.len(), 1);
        assert!(!recovered[0].firing);
    }

    #[test]
    fn an_unknown_reading_neither_trips_nor_clears() {
        let config = config();
        let mut state = AlertState::default();

        // Nothing has reported yet.
        for _ in 0..5 {
            assert!(evaluate(&mut state, &config, &Sample::default(), 0).is_empty());
        }
        assert!(!state.is_tripped(AlertKind::LowPeers));

        // Trip it, then lose the source: the alert stays up rather than
        // silently resolving on missing data.
        evaluate(&mut state, &config, &peers(1), 0);
        evaluate(&mut state, &config, &peers(1), 0);
        assert!(state.is_tripped(AlertKind::LowPeers));

        for _ in 0..5 {
            assert!(evaluate(&mut state, &config, &Sample::default(), 0).is_empty());
        }
        assert!(state.is_tripped(AlertKind::LowPeers));
    }

    #[test]
    fn an_unconfigured_threshold_is_never_evaluated() {
        let config = AlertConfig::default();
        let mut state = AlertState::default();

        let sample = Sample {
            secs_since_block: Some(9999),
            finalized_lag: Some(9999),
            peers: Some(0),
            disk_pct: Some(100.0),
        };
        for _ in 0..5 {
            assert!(evaluate(&mut state, &config, &sample, 0).is_empty());
        }
        assert!(!config.is_enabled());
        assert!(!state.is_tripped(AlertKind::DiskFull));
    }

    #[test]
    fn a_stalled_websocket_trips_the_no_block_threshold() {
        let config = AlertConfig {
            no_block_secs: Some(30),
            confirm_samples: 1,
            cooldown_secs: 0,
            ..AlertConfig::default()
        };
        let mut state = AlertState::default();

        let flowing = Sample {
            secs_since_block: Some(1),
            ..Sample::default()
        };
        assert!(evaluate(&mut state, &config, &flowing, 0).is_empty());

        let stalled = Sample {
            secs_since_block: Some(31),
            ..Sample::default()
        };
        let fired = evaluate(&mut state, &config, &stalled, 0);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].kind, AlertKind::NoBlock);
        assert!(fired[0].firing);

        let recovered = evaluate(&mut state, &config, &flowing, 0);
        assert_eq!(recovered.len(), 1);
        assert!(!recovered[0].firing);
    }

    #[test]
    fn thresholds_are_independent() {
        let config = AlertConfig {
            min_peers: Some(5),
            disk_pct: Some(80.0),
            confirm_samples: 1,
            cooldown_secs: 0,
            ..AlertConfig::default()
        };
        let mut state = AlertState::default();

        let sample = Sample {
            peers: Some(1),
            disk_pct: Some(90.0),
            ..Sample::default()
        };
        let fired = evaluate(&mut state, &config, &sample, 0);
        assert_eq!(fired.len(), 2);
        assert!(state.is_tripped(AlertKind::LowPeers));
        assert!(state.is_tripped(AlertKind::DiskFull));

        // One clears, the other stays.
        let sample = Sample {
            peers: Some(50),
            disk_pct: Some(90.0),
            ..Sample::default()
        };
        let fired = evaluate(&mut state, &config, &sample, 0);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].kind, AlertKind::LowPeers);
        assert!(state.is_tripped(AlertKind::DiskFull));
    }

    #[test]
    fn the_payload_carries_both_discord_and_slack_fields() {
        let transition = Transition {
            kind: AlertKind::LowPeers,
            firing: true,
            value: "1 peers".to_string(),
            threshold: "5 peers".to_string(),
        };
        let payload = transition.payload("MFNode");

        // Discord renders `content`, Slack renders `text`; both must be there
        // and identical, or one of the two endpoints rejects the body.
        assert_eq!(payload["content"], payload["text"]);
        assert!(payload["content"].as_str().unwrap().contains("MFNode"));
        assert_eq!(payload["alert"], "low_peers");
        assert_eq!(payload["status"], "firing");

        let resolved = Transition {
            firing: false,
            ..transition
        };
        assert_eq!(resolved.payload("MFNode")["status"], "resolved");
    }

    #[test]
    fn flags_fill_the_config() {
        let mut config = AlertConfig::default();
        config
            .apply("--webhook-url", "https://example.com/hook")
            .unwrap();
        config.apply("--alert-no-block", "45").unwrap();
        config.apply("--alert-min-peers", "8").unwrap();
        config.apply("--alert-disk", "85").unwrap();

        assert_eq!(
            config.webhook_url.as_deref(),
            Some("https://example.com/hook")
        );
        assert_eq!(config.no_block_secs, Some(45));
        assert_eq!(config.min_peers, Some(8));
        assert_eq!(config.disk_pct, Some(85.0));
        assert_eq!(config.finalized_lag, None);
        assert_eq!(config.cooldown_secs, 300);
        assert!(config.is_enabled());
        assert!(config.is_configured());

        config.apply("--alert-cooldown", "60").unwrap();
        assert_eq!(config.cooldown_secs, 60);
    }

    #[test]
    fn a_webhook_alone_counts_as_configured_but_arms_nothing() {
        let mut config = AlertConfig::default();
        config
            .apply("--webhook-url", "https://example.com/hook")
            .unwrap();

        // Nothing to evaluate, but the flag was given, so headless mode should
        // still refuse it rather than silently drop it.
        assert!(!config.is_enabled());
        assert!(config.is_configured());
    }

    #[test]
    fn bad_values_are_reported() {
        let mut config = AlertConfig::default();
        assert!(config.apply("--alert-min-peers", "lots").is_err());
        assert!(config.apply("--alert-confirm", "0").is_err());

        // A placeholder left in place would post into the void otherwise.
        assert!(config
            .apply("--webhook-url", "DISCORD_WEBHOOK_URL")
            .is_err());
    }

    #[test]
    fn only_alert_flags_are_claimed() {
        assert!(is_alert_flag("--alert-min-peers"));
        assert!(is_alert_flag("--webhook-url"));

        // The argument loop owns these; claiming them here would swallow flags
        // this module knows nothing about.
        assert!(!is_alert_flag("--metrics-url"));
        assert!(!is_alert_flag("--json"));
        assert!(!is_alert_flag("--alert-noblock"));

        assert!(AlertConfig::default().apply("--metrics-url", "x").is_err());
    }
}
