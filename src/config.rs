use std::env;
use std::fs;

const DEFAULT_METRICS_URL: &str = "http://localhost:8889/metrics";
const DEFAULT_WS_URL: &str = "ws://localhost:8081";
const DEFAULT_NETWORK: &str = "mainnet";
const DEFAULT_REFRESH_SECS: u64 = 1;

#[derive(Debug, Clone)]
pub struct Config {
    pub metrics_url: String,
    pub ws_url: String,
    pub refresh_secs: u64,
    pub network: String,
    pub external_rpc_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            metrics_url: DEFAULT_METRICS_URL.to_string(),
            ws_url: DEFAULT_WS_URL.to_string(),
            refresh_secs: DEFAULT_REFRESH_SECS,
            network: DEFAULT_NETWORK.to_string(),
            external_rpc_url: None,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let mut cfg = Self::default();
        if let Some(pairs) = load_config_file()? {
            apply_file_config(&mut cfg, pairs)?;
        }
        parse_args(&mut cfg)?;
        validate_url(&cfg.metrics_url, "--metrics-url")?;
        validate_url(&cfg.ws_url, "--ws-url")?;
        if let Some(ref url) = cfg.external_rpc_url {
            validate_url(url, "--external-rpc-url")?;
        }
        Ok(cfg)
    }

    // --external-rpc-url takes precedence over --network
    pub fn resolved_external_rpc_url(&self) -> String {
        match &self.external_rpc_url {
            Some(url) => url.clone(),
            None => format!("wss://rpc-{}.monadinfra.com", self.network),
        }
    }
}

fn config_file_path() -> Option<std::path::PathBuf> {
    let base = env::var("XDG_CONFIG_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env::var("HOME").ok().map(|h| {
                let mut p = std::path::PathBuf::from(h);
                p.push(".config");
                p
            })
        })?;
    let mut path = base;
    path.push("monad-monitor");
    path.push("config.toml");
    Some(path)
}

// Returns None if file not found, Some(pairs) if found and parsed.
fn load_config_file() -> Result<Option<Vec<(String, String)>>, String> {
    let path = match config_file_path() {
        Some(p) => p,
        None => return Ok(None),
    };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("config: cannot read {}: {}", path.display(), e)),
    };
    let mut pairs = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let eq = line.find('=').ok_or_else(|| {
            format!(
                "config: malformed line {} (expected key = value): {:?}",
                lineno + 1,
                line
            )
        })?;
        let key = line[..eq].trim().to_string();
        let val = line[eq + 1..].trim().trim_matches('"').to_string();
        pairs.push((key, val));
    }
    Ok(Some(pairs))
}

fn apply_file_config(cfg: &mut Config, pairs: Vec<(String, String)>) -> Result<(), String> {
    for (key, val) in pairs {
        match key.as_str() {
            "metrics_url" => cfg.metrics_url = val,
            "ws_url" => cfg.ws_url = val,
            "network" => cfg.network = val,
            "external_rpc_url" => cfg.external_rpc_url = Some(val),
            "refresh" => {
                cfg.refresh_secs = val
                    .parse::<u64>()
                    .map_err(|_| format!("config: invalid value for refresh: {:?}", val))?;
            }
            // Unknown keys are silently ignored for forward compatibility.
            _ => {}
        }
    }
    Ok(())
}

fn parse_args(cfg: &mut Config) -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--metrics-url" => {
                cfg.metrics_url = next_arg(&args, &mut i, "--metrics-url")?;
            }
            "--ws-url" => {
                cfg.ws_url = next_arg(&args, &mut i, "--ws-url")?;
            }
            "--refresh" => {
                let v = next_arg(&args, &mut i, "--refresh")?;
                cfg.refresh_secs = v
                    .parse::<u64>()
                    .map_err(|_| format!("--refresh: expected a positive integer, got {:?}", v))?;
            }
            "--network" => {
                cfg.network = next_arg(&args, &mut i, "--network")?;
            }
            "--external-rpc-url" => {
                cfg.external_rpc_url = Some(next_arg(&args, &mut i, "--external-rpc-url")?);
            }
            other => {
                return Err(format!("unknown flag: {}", other));
            }
        }
        i += 1;
    }
    Ok(())
}

fn next_arg(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{}: missing value", flag))
}

fn validate_url(url: &str, flag: &str) -> Result<(), String> {
    // Minimal check: must have a scheme and "://"
    if !url.contains("://") {
        return Err(format!("{}: invalid URL (no scheme): {:?}", flag, url));
    }
    Ok(())
}

fn print_help() {
    println!(
        "monad-monitor - Monad node TUI monitor

USAGE:
    monad-monitor [OPTIONS]

OPTIONS:
    --metrics-url <URL>       Prometheus metrics endpoint
                              (default: http://localhost:8889/metrics)
    --ws-url <URL>            Local node WebSocket endpoint
                              (default: ws://localhost:8081)
    --refresh <seconds>       Metrics polling interval in seconds (default: 1)
    --network <name>          Network name for external RPC comparison URL
                              wss://rpc-<name>.monadinfra.com (default: mainnet)
    --external-rpc-url <URL>  Override the external RPC URL directly;
                              takes precedence over --network
    -h, --help                Print this help

CONFIG FILE:
    Optional config file is read from:
      $XDG_CONFIG_HOME/monad-monitor/config.toml
      ~/.config/monad-monitor/config.toml  (fallback)

    Supported keys: metrics_url, ws_url, refresh, network, external_rpc_url
    CLI flags take precedence over the config file.
    Unknown keys are ignored.

KEYBOARD:
    q / Q / Esc   Quit
    t / T         Cycle themes
"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_cfg() -> Config {
        Config::default()
    }

    #[test]
    fn test_defaults() {
        let cfg = default_cfg();
        assert_eq!(cfg.metrics_url, "http://localhost:8889/metrics");
        assert_eq!(cfg.ws_url, "ws://localhost:8081");
        assert_eq!(cfg.refresh_secs, 1);
        assert_eq!(cfg.network, "mainnet");
        assert!(cfg.external_rpc_url.is_none());
    }

    #[test]
    fn test_resolved_external_rpc_url_default() {
        let cfg = default_cfg();
        assert_eq!(
            cfg.resolved_external_rpc_url(),
            "wss://rpc-mainnet.monadinfra.com"
        );
    }

    #[test]
    fn test_resolved_external_rpc_url_network() {
        let mut cfg = default_cfg();
        cfg.network = "testnet".to_string();
        assert_eq!(
            cfg.resolved_external_rpc_url(),
            "wss://rpc-testnet.monadinfra.com"
        );
    }

    #[test]
    fn test_resolved_external_rpc_url_override() {
        let mut cfg = default_cfg();
        cfg.network = "testnet".to_string();
        cfg.external_rpc_url = Some("wss://my-rpc.example.com".to_string());
        assert_eq!(cfg.resolved_external_rpc_url(), "wss://my-rpc.example.com");
    }

    #[test]
    fn test_file_config_overrides_defaults() {
        let mut cfg = default_cfg();
        let pairs = vec![
            (
                "metrics_url".to_string(),
                "http://node:8889/metrics".to_string(),
            ),
            ("ws_url".to_string(), "ws://node:8081".to_string()),
            ("refresh".to_string(), "5".to_string()),
            ("network".to_string(), "testnet".to_string()),
        ];
        apply_file_config(&mut cfg, pairs).unwrap();
        assert_eq!(cfg.metrics_url, "http://node:8889/metrics");
        assert_eq!(cfg.ws_url, "ws://node:8081");
        assert_eq!(cfg.refresh_secs, 5);
        assert_eq!(cfg.network, "testnet");
    }

    #[test]
    fn test_file_config_external_rpc_url() {
        let mut cfg = default_cfg();
        let pairs = vec![(
            "external_rpc_url".to_string(),
            "wss://custom.example.com".to_string(),
        )];
        apply_file_config(&mut cfg, pairs).unwrap();
        assert_eq!(
            cfg.external_rpc_url,
            Some("wss://custom.example.com".to_string())
        );
    }

    #[test]
    fn test_file_config_unknown_keys_ignored() {
        let mut cfg = default_cfg();
        let pairs = vec![
            ("unknown_key".to_string(), "value".to_string()),
            (
                "metrics_url".to_string(),
                "http://x:8889/metrics".to_string(),
            ),
        ];
        apply_file_config(&mut cfg, pairs).unwrap();
        assert_eq!(cfg.metrics_url, "http://x:8889/metrics");
    }

    #[test]
    fn test_file_config_bad_refresh() {
        let mut cfg = default_cfg();
        let pairs = vec![("refresh".to_string(), "notanumber".to_string())];
        assert!(apply_file_config(&mut cfg, pairs).is_err());
    }

    #[test]
    fn test_parse_config_file_text() {
        // Simulate load_config_file parsing logic inline
        let text =
            "# comment\nmetrics_url = http://a:8889/metrics\nws_url = ws://a:8081\n\nrefresh = 3\n";
        let mut pairs = Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let eq = line.find('=').unwrap_or_else(|| panic!("line {}", lineno));
            let key = line[..eq].trim().to_string();
            let val = line[eq + 1..].trim().trim_matches('"').to_string();
            pairs.push((key, val));
        }
        assert_eq!(pairs.len(), 3);
        assert_eq!(
            pairs[0],
            (
                "metrics_url".to_string(),
                "http://a:8889/metrics".to_string()
            )
        );
        assert_eq!(pairs[2], ("refresh".to_string(), "3".to_string()));
    }

    #[test]
    fn test_parse_config_file_malformed_line() {
        let text = "no_equals_sign\n";
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert!(line.find('=').is_none(), "should be malformed");
        }
    }

    #[test]
    fn test_validate_url_ok() {
        assert!(validate_url("http://localhost:8889/metrics", "--metrics-url").is_ok());
        assert!(validate_url("ws://localhost:8081", "--ws-url").is_ok());
        assert!(validate_url("wss://rpc-mainnet.monadinfra.com", "--external-rpc-url").is_ok());
    }

    #[test]
    fn test_validate_url_bad() {
        assert!(validate_url("localhost:8889", "--metrics-url").is_err());
        assert!(validate_url("not-a-url", "--ws-url").is_err());
    }
}
