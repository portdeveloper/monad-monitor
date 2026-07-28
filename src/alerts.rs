use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Alert thresholds and webhook settings. Every threshold is off by default;
/// with nothing configured the monitor behaves exactly as before.
#[derive(Debug, Clone, Default)]
pub struct AlertConfig {
    /// Fire when no new block has been seen for this many seconds.
    pub block_stall_secs: Option<u64>,
    /// Fire when the finalized lag exceeds this many blocks.
    pub finalized_lag: Option<u64>,
    /// Fire when the peer count drops below this.
    pub min_peers: Option<u64>,
    /// Fire when disk usage exceeds this percentage.
    pub disk_pct: Option<f64>,
    /// Webhook URL that receives a JSON POST on every alert transition.
    pub webhook_url: Option<String>,
}

impl AlertConfig {
    /// Read the optional config file, then apply CLI flags on top (flags win).
    pub fn load() -> Result<Self, String> {
        let file_text = config_file_text()?;
        let args: Vec<String> = env::args().skip(1).collect();
        Self::from_sources(file_text.as_deref(), &args)
    }

    pub fn enabled(&self) -> bool {
        self.block_stall_secs.is_some()
            || self.finalized_lag.is_some()
            || self.min_peers.is_some()
            || self.disk_pct.is_some()
    }

    fn from_sources(file_text: Option<&str>, args: &[String]) -> Result<Self, String> {
        let mut cfg = Self::default();
        if let Some(text) = file_text {
            cfg.apply_file(text)?;
        }
        cfg.apply_args(args)?;
        Ok(cfg)
    }

    fn apply_file(&mut self, text: &str) -> Result<(), String> {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let eq = match line.find('=') {
                Some(pos) => pos,
                None => continue,
            };
            let key = line[..eq].trim();
            let val = line[eq + 1..].trim().trim_matches('"');
            match key {
                "alert_block_stall" => {
                    self.block_stall_secs = Some(parse_positive_u64(val, "alert_block_stall")?);
                }
                "alert_finalized_lag" => {
                    self.finalized_lag = Some(parse_u64(val, "alert_finalized_lag")?);
                }
                "alert_min_peers" => {
                    self.min_peers = Some(parse_positive_u64(val, "alert_min_peers")?);
                }
                "alert_disk_pct" => {
                    self.disk_pct = Some(parse_pct(val, "alert_disk_pct")?);
                }
                "webhook_url" => {
                    self.webhook_url = Some(validate_url(val, "webhook_url")?);
                }
                // Keys owned by other features are left alone.
                _ => {}
            }
        }
        Ok(())
    }

    fn apply_args(&mut self, args: &[String]) -> Result<(), String> {
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--alert-block-stall" => {
                    let v = take_value(args, &mut i, "--alert-block-stall")?;
                    self.block_stall_secs = Some(parse_positive_u64(&v, "--alert-block-stall")?);
                }
                "--alert-finalized-lag" => {
                    let v = take_value(args, &mut i, "--alert-finalized-lag")?;
                    self.finalized_lag = Some(parse_u64(&v, "--alert-finalized-lag")?);
                }
                "--alert-min-peers" => {
                    let v = take_value(args, &mut i, "--alert-min-peers")?;
                    self.min_peers = Some(parse_positive_u64(&v, "--alert-min-peers")?);
                }
                "--alert-disk-pct" => {
                    let v = take_value(args, &mut i, "--alert-disk-pct")?;
                    self.disk_pct = Some(parse_pct(&v, "--alert-disk-pct")?);
                }
                "--webhook-url" => {
                    let v = take_value(args, &mut i, "--webhook-url")?;
                    self.webhook_url = Some(validate_url(&v, "--webhook-url")?);
                }
                // Flags owned by other parsers are left alone so the alert
                // flags compose with endpoint flags and the like.
                _ => {}
            }
            i += 1;
        }
        Ok(())
    }
}

/// One snapshot of the values the alert thresholds look at. `None` means the
/// data source has not delivered yet and the alert is skipped, so the monitor
/// never fires from startup defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlertSample {
    pub secs_since_last_block: Option<f64>,
    pub finalized_lag: Option<u64>,
    pub peer_count: Option<u64>,
    pub disk_used_pct: Option<f64>,
}

/// Which alerts are currently firing. The UI turns the affected cells red.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActiveAlerts {
    pub block_stall: bool,
    pub finalized_lag: bool,
    pub min_peers: bool,
    pub disk_pct: bool,
}

/// A single ok -> alert or alert -> ok transition.
#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub name: &'static str,
    pub firing: bool,
    pub value: f64,
    pub threshold: f64,
    pub detail: String,
}

/// Tracks per-alert state and reports transitions.
pub struct AlertEngine {
    config: AlertConfig,
    active: ActiveAlerts,
}

impl AlertEngine {
    pub fn new(config: AlertConfig) -> Self {
        Self {
            config,
            active: ActiveAlerts::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled()
    }

    pub fn webhook_url(&self) -> Option<&str> {
        self.config.webhook_url.as_deref()
    }

    pub fn active(&self) -> ActiveAlerts {
        self.active
    }

    /// Compare a sample against the configured thresholds and return one event
    /// per state transition. Alerts fire on ok -> alert and resolve on
    /// alert -> ok, never on repeats, so one incident produces exactly two
    /// events no matter how many refresh ticks it lasts.
    pub fn evaluate(&mut self, sample: &AlertSample) -> Vec<AlertEvent> {
        let mut events = Vec::new();

        if let (Some(threshold), Some(secs)) =
            (self.config.block_stall_secs, sample.secs_since_last_block)
        {
            let tripped = secs >= threshold as f64;
            if tripped != self.active.block_stall {
                self.active.block_stall = tripped;
                events.push(AlertEvent {
                    name: "block_stall",
                    firing: tripped,
                    value: secs,
                    threshold: threshold as f64,
                    detail: format!("no new block for {:.1}s (threshold {}s)", secs, threshold),
                });
            }
        }

        if let (Some(threshold), Some(lag)) = (self.config.finalized_lag, sample.finalized_lag) {
            let tripped = lag > threshold;
            if tripped != self.active.finalized_lag {
                self.active.finalized_lag = tripped;
                events.push(AlertEvent {
                    name: "finalized_lag",
                    firing: tripped,
                    value: lag as f64,
                    threshold: threshold as f64,
                    detail: format!("finalized lag {} blocks (threshold {})", lag, threshold),
                });
            }
        }

        if let (Some(threshold), Some(peers)) = (self.config.min_peers, sample.peer_count) {
            let tripped = peers < threshold;
            if tripped != self.active.min_peers {
                self.active.min_peers = tripped;
                events.push(AlertEvent {
                    name: "min_peers",
                    firing: tripped,
                    value: peers as f64,
                    threshold: threshold as f64,
                    detail: format!("{} peers (threshold min {})", peers, threshold),
                });
            }
        }

        if let (Some(threshold), Some(pct)) = (self.config.disk_pct, sample.disk_used_pct) {
            let tripped = pct > threshold;
            if tripped != self.active.disk_pct {
                self.active.disk_pct = tripped;
                events.push(AlertEvent {
                    name: "disk_pct",
                    firing: tripped,
                    value: pct,
                    threshold,
                    detail: format!("disk {:.1}% used (threshold {}%)", pct, threshold),
                });
            }
        }

        events
    }
}

/// Build the webhook payload for one alert event. `text` and `content` carry
/// the same human-readable summary so the payload renders as a message when
/// pointed directly at a Slack or Discord webhook URL.
pub fn payload(event: &AlertEvent, node: &str, timestamp: u64) -> Value {
    let state = if event.firing { "firing" } else { "resolved" };
    let text = format!(
        "[monad-monitor] {} {} on {}: {}",
        event.name, state, node, event.detail
    );
    json!({
        "alert": event.name,
        "state": state,
        "value": json_num(event.value),
        "threshold": json_num(event.threshold),
        "node": node,
        "timestamp": timestamp,
        "text": text,
        "content": text,
    })
}

// Render whole numbers without a trailing .0 so counts stay counts in JSON.
fn json_num(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

/// POST one alert event to the webhook. Failures are returned as strings so
/// the caller can show them in the footer; they never affect monitoring.
pub async fn post_webhook(
    client: &reqwest::Client,
    url: &str,
    event: &AlertEvent,
    node: &str,
) -> Result<(), String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = payload(event, node, timestamp);
    client
        .post(url)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn config_file_path() -> Option<PathBuf> {
    let base = env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            env::var("HOME").ok().map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".config");
                p
            })
        })?;
    let mut path = base;
    path.push("monad-monitor");
    path.push("config.toml");
    Some(path)
}

fn config_file_text() -> Result<Option<String>, String> {
    let path = match config_file_path() {
        Some(p) => p,
        None => return Ok(None),
    };
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("config: cannot read {}: {}", path.display(), e)),
    }
}

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{}: missing value", flag))
}

fn parse_u64(val: &str, what: &str) -> Result<u64, String> {
    val.parse::<u64>()
        .map_err(|_| format!("{}: expected a whole number, got {:?}", what, val))
}

fn parse_positive_u64(val: &str, what: &str) -> Result<u64, String> {
    let n = parse_u64(val, what)?;
    if n == 0 {
        return Err(format!("{}: must be at least 1", what));
    }
    Ok(n)
}

fn parse_pct(val: &str, what: &str) -> Result<f64, String> {
    let n: f64 = val
        .parse()
        .map_err(|_| format!("{}: expected a number, got {:?}", what, val))?;
    if n <= 0.0 || n > 100.0 {
        return Err(format!(
            "{}: expected a percentage between 0 and 100, got {}",
            what, val
        ));
    }
    Ok(n)
}

fn validate_url(url: &str, what: &str) -> Result<String, String> {
    if !url.contains("://") {
        return Err(format!("{}: invalid URL (no scheme): {:?}", what, url));
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_all() -> AlertConfig {
        AlertConfig {
            block_stall_secs: Some(30),
            finalized_lag: Some(10),
            min_peers: Some(5),
            disk_pct: Some(90.0),
            webhook_url: Some("http://localhost:9999/hook".to_string()),
        }
    }

    fn healthy_sample() -> AlertSample {
        AlertSample {
            secs_since_last_block: Some(1.0),
            finalized_lag: Some(2),
            peer_count: Some(40),
            disk_used_pct: Some(50.0),
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_thresholds_never_fires() {
        let mut engine = AlertEngine::new(AlertConfig::default());
        let sample = AlertSample {
            secs_since_last_block: Some(9999.0),
            finalized_lag: Some(9999),
            peer_count: Some(0),
            disk_used_pct: Some(100.0),
        };
        assert!(!engine.enabled());
        assert!(engine.evaluate(&sample).is_empty());
    }

    #[test]
    fn webhook_url_alone_enables_nothing() {
        let cfg = AlertConfig {
            webhook_url: Some("http://localhost:1/hook".to_string()),
            ..AlertConfig::default()
        };
        assert!(!cfg.enabled());
    }

    #[test]
    fn fires_once_on_trip_and_once_on_recovery() {
        let mut engine = AlertEngine::new(cfg_all());
        assert!(engine.evaluate(&healthy_sample()).is_empty());

        let mut stalled = healthy_sample();
        stalled.secs_since_last_block = Some(45.0);
        let events = engine.evaluate(&stalled);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "block_stall");
        assert!(events[0].firing);
        assert_eq!(events[0].value, 45.0);
        assert_eq!(events[0].threshold, 30.0);
        assert!(engine.active().block_stall);

        // Condition persists: no repeat notifications
        for _ in 0..100 {
            assert!(engine.evaluate(&stalled).is_empty());
        }

        let events = engine.evaluate(&healthy_sample());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "block_stall");
        assert!(!events[0].firing);
        assert!(!engine.active().block_stall);
        assert!(engine.evaluate(&healthy_sample()).is_empty());
    }

    #[test]
    fn each_threshold_trips_on_its_own_metric() {
        let mut engine = AlertEngine::new(cfg_all());
        assert!(engine.evaluate(&healthy_sample()).is_empty());

        let mut bad = healthy_sample();
        bad.finalized_lag = Some(11);
        bad.peer_count = Some(2);
        bad.disk_used_pct = Some(95.5);
        let events = engine.evaluate(&bad);
        let names: Vec<&str> = events.iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["finalized_lag", "min_peers", "disk_pct"]);
        assert!(events.iter().all(|e| e.firing));
        assert!(engine.active().finalized_lag);
        assert!(engine.active().min_peers);
        assert!(engine.active().disk_pct);
        assert!(!engine.active().block_stall);

        let events = engine.evaluate(&healthy_sample());
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| !e.firing));
    }

    #[test]
    fn boundary_values_do_not_trip() {
        let mut engine = AlertEngine::new(cfg_all());
        let mut edge = healthy_sample();
        edge.finalized_lag = Some(10); // threshold is >, not >=
        edge.peer_count = Some(5); // threshold is <, not <=
        edge.disk_used_pct = Some(90.0); // threshold is >, not >=
        assert!(engine.evaluate(&edge).is_empty());
    }

    #[test]
    fn missing_data_is_not_evaluated() {
        let mut engine = AlertEngine::new(cfg_all());
        // No data source has delivered yet: nothing fires at startup
        assert!(engine.evaluate(&AlertSample::default()).is_empty());
    }

    #[test]
    fn payload_shape() {
        let event = AlertEvent {
            name: "min_peers",
            firing: true,
            value: 2.0,
            threshold: 5.0,
            detail: "2 peers (threshold min 5)".to_string(),
        };
        let body = payload(&event, "node-1", 1700000000);
        assert_eq!(body["alert"], "min_peers");
        assert_eq!(body["state"], "firing");
        assert_eq!(body["value"], 2);
        assert_eq!(body["threshold"], 5);
        assert_eq!(body["node"], "node-1");
        assert_eq!(body["timestamp"], 1700000000u64);
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("min_peers"));
        assert!(text.contains("firing"));
        assert!(text.contains("node-1"));
        assert!(text.contains("2 peers (threshold min 5)"));
        assert_eq!(body["content"], body["text"]);
    }

    #[test]
    fn payload_resolved_and_fractional_value() {
        let event = AlertEvent {
            name: "block_stall",
            firing: false,
            value: 0.4,
            threshold: 30.0,
            detail: "no new block for 0.4s (threshold 30s)".to_string(),
        };
        let body = payload(&event, "node-1", 1700000000);
        assert_eq!(body["state"], "resolved");
        assert_eq!(body["value"], 0.4);
        assert_eq!(body["threshold"], 30);
        assert!(body["text"].as_str().unwrap().contains("resolved"));
    }

    #[test]
    fn config_from_flags() {
        let cfg = AlertConfig::from_sources(
            None,
            &args(&[
                "--alert-min-peers",
                "5",
                "--alert-disk-pct",
                "90",
                "--webhook-url",
                "http://localhost:1234/x",
                "--metrics-url",
                "http://other:8889/metrics",
            ]),
        )
        .unwrap();
        assert_eq!(cfg.min_peers, Some(5));
        assert_eq!(cfg.disk_pct, Some(90.0));
        assert_eq!(cfg.webhook_url.as_deref(), Some("http://localhost:1234/x"));
        // Flags owned by other parsers are ignored, not errors
        assert_eq!(cfg.block_stall_secs, None);
        assert_eq!(cfg.finalized_lag, None);
    }

    #[test]
    fn config_flag_errors() {
        assert!(AlertConfig::from_sources(None, &args(&["--alert-min-peers"])).is_err());
        assert!(AlertConfig::from_sources(None, &args(&["--alert-min-peers", "abc"])).is_err());
        assert!(AlertConfig::from_sources(None, &args(&["--alert-min-peers", "0"])).is_err());
        assert!(AlertConfig::from_sources(None, &args(&["--alert-disk-pct", "150"])).is_err());
        assert!(AlertConfig::from_sources(None, &args(&["--webhook-url", "not-a-url"])).is_err());
    }

    #[test]
    fn config_from_file_and_flag_precedence() {
        let file = concat!(
            "# alerts\n",
            "alert_min_peers = 5\n",
            "alert_block_stall = 30\n",
            "webhook_url = \"http://localhost:1/hook\"\n",
            "metrics_url = \"http://other:8889/metrics\"\n",
        );
        let cfg = AlertConfig::from_sources(Some(file), &[]).unwrap();
        assert_eq!(cfg.min_peers, Some(5));
        assert_eq!(cfg.block_stall_secs, Some(30));
        assert_eq!(cfg.webhook_url.as_deref(), Some("http://localhost:1/hook"));

        // CLI flags override the config file
        let cfg =
            AlertConfig::from_sources(Some(file), &args(&["--alert-min-peers", "8"])).unwrap();
        assert_eq!(cfg.min_peers, Some(8));
    }
}
