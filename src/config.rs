//! Endpoint and refresh settings.
//!
//! The metrics and WebSocket endpoints used to be constants, so anyone running
//! the monitor from another host, on other ports, or against a remote node had
//! to patch the source. Settings now come from three layers, each overriding
//! the one before: the built-in defaults, an optional config file, then the
//! command line. The defaults are the values that used to be hardcoded, so a
//! run with no flags and no file behaves exactly as it did.
//!
//! A bad value is reported against where it came from, naming the flag or the
//! file and the line. Falling back to a default instead would leave an
//! operator watching localhost while they believe they configured a remote
//! node.

use std::path::PathBuf;

/// Settings, spelled as config file keys. Each one is also a flag:
/// `metrics_url` is `--metrics-url`. Keeping one list is what stops the two
/// spellings from drifting apart.
const KEYS: [&str; 5] = [
    "metrics_url",
    "ws_url",
    "refresh",
    "network",
    "external_rpc_url",
];

/// An hour is already longer than any monitoring interval, and an unbounded
/// period overflows the timer it is handed to.
const MAX_REFRESH_SECS: u64 = 3600;

/// What a run is configured with, once the layers are merged.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Prometheus endpoint scraped for node metrics.
    pub metrics_url: String,
    /// The node's WebSocket, subscribed to for new blocks.
    pub ws_url: String,
    /// How often the metrics endpoint is scraped.
    pub refresh_secs: u64,
    /// Network name. It builds the public endpoint the local block height is
    /// compared against, and labels the JSON snapshot.
    pub network: String,
    /// That comparison endpoint, when it is given outright rather than built
    /// from the network name.
    pub external_rpc_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            metrics_url: "http://localhost:8889/metrics".to_string(),
            ws_url: "ws://localhost:8081".to_string(),
            refresh_secs: 1,
            network: "mainnet".to_string(),
            external_rpc_url: None,
        }
    }
}

impl Config {
    /// Defaults, then the config file, then the command line. Layers merge
    /// field by field, so a file that only sets `ws_url` leaves the metrics
    /// endpoint on its default, and one flag does not discard the file.
    pub fn resolve(file: Layer, flags: Layer) -> Self {
        let mut cfg = Self::default();
        for layer in [file, flags] {
            if let Some(url) = layer.metrics_url {
                cfg.metrics_url = url;
            }
            if let Some(url) = layer.ws_url {
                cfg.ws_url = url;
            }
            if let Some(secs) = layer.refresh_secs {
                cfg.refresh_secs = secs;
            }
            if let Some(name) = layer.network {
                cfg.network = name;
            }
            if let Some(url) = layer.external_rpc_url {
                cfg.external_rpc_url = Some(url);
            }
        }
        cfg
    }

    /// The endpoint the block-difference panel compares the local height
    /// against. An explicit URL wins over the network name: it is the more
    /// specific of the two, and the only way to reach a node that is not on
    /// monadinfra.
    pub fn resolved_external_rpc_url(&self) -> String {
        match &self.external_rpc_url {
            Some(url) => url.clone(),
            None => format!("wss://rpc-{}.monadinfra.com", self.network),
        }
    }
}

/// One layer of settings: what the config file said, or what the command line
/// said. A field stays `None` until that layer sets it, which is what lets a
/// flag win over the file without either knowing the other's values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layer {
    metrics_url: Option<String>,
    ws_url: Option<String>,
    refresh_secs: Option<u64>,
    network: Option<String>,
    external_rpc_url: Option<String>,
}

impl Layer {
    /// Fills one setting in from a command-line flag. The caller owns the
    /// argument loop, so this stays a single-flag operation: an unknown flag is
    /// the caller's error to report, which is what makes a typo fail loudly
    /// instead of never taking effect.
    pub fn apply_flag(&mut self, flag: &str, value: &str) -> Result<(), String> {
        match key_of(flag) {
            Some(key) => self.set(key, value, flag),
            None => Err(format!("not an endpoint flag: {}", flag)),
        }
    }

    /// Whether this layer set the refresh interval. The snapshot mode has its
    /// own interval flag, so a `--refresh` typed there is reported rather than
    /// ignored, while the same key sitting in a config file is simply not used
    /// by that mode.
    pub fn sets_refresh(&self) -> bool {
        self.refresh_secs.is_some()
    }

    /// `origin` is what a bad value gets blamed on: a flag, or a key and its
    /// line in the config file.
    fn set(&mut self, key: &str, value: &str, origin: &str) -> Result<(), String> {
        match key {
            "metrics_url" => {
                require_scheme(origin, value, &["http://", "https://"])?;
                self.metrics_url = Some(value.to_string());
            }
            "ws_url" => {
                require_scheme(origin, value, &["ws://", "wss://"])?;
                self.ws_url = Some(value.to_string());
            }
            "external_rpc_url" => {
                require_scheme(origin, value, &["ws://", "wss://"])?;
                self.external_rpc_url = Some(value.to_string());
            }
            "network" => {
                // The name is spliced into a hostname, so anything that cannot
                // appear in one would only surface later as a failed
                // connection to a nonsense URL.
                let usable = !value.is_empty()
                    && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
                if !usable {
                    return Err(format!(
                        "{} expects a network name like mainnet or testnet, got {:?}",
                        origin, value
                    ));
                }
                self.network = Some(value.to_string());
            }
            "refresh" => {
                let secs: u64 = value.parse().map_err(|_| {
                    format!(
                        "{} expects a whole number of seconds, got {:?}",
                        origin, value
                    )
                })?;
                if !(1..=MAX_REFRESH_SECS).contains(&secs) {
                    return Err(format!(
                        "{} expects 1 to {} seconds, got {}",
                        origin, MAX_REFRESH_SECS, secs
                    ));
                }
                self.refresh_secs = Some(secs);
            }
            _ => return Err(format!("unknown setting: {}", key)),
        }
        Ok(())
    }
}

fn require_scheme(origin: &str, value: &str, schemes: &[&str]) -> Result<(), String> {
    if schemes.iter().any(|scheme| value.starts_with(scheme)) {
        return Ok(());
    }
    Err(format!(
        "{} must start with {}, got {:?}",
        origin,
        schemes.join(" or "),
        value
    ))
}

/// The flag that carries a key: `metrics_url` arrives as `--metrics-url`.
fn flag_of(key: &str) -> String {
    format!("--{}", key.replace('_', "-"))
}

/// The key a flag carries, or `None` when the flag is not one of ours. The
/// spelling has to match exactly, so `--metrics_url` stays an unknown argument
/// rather than becoming a second accepted form.
fn key_of(flag: &str) -> Option<&'static str> {
    KEYS.into_iter().find(|key| flag_of(key) == flag)
}

/// Whether the argument loop should hand this flag to [`Layer::apply_flag`].
/// Every one of them takes a value.
pub fn is_config_flag(flag: &str) -> bool {
    key_of(flag).is_some()
}

/// Where the config file lives, given the environment: under
/// `XDG_CONFIG_HOME` when that is set, otherwise under `~/.config`. `None`
/// means neither is set, which is a host with nowhere to look.
fn path_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let dir = match xdg {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => home?.join(".config"),
    };
    Some(dir.join("monad-monitor").join("config.toml"))
}

/// `$XDG_CONFIG_HOME/monad-monitor/config.toml`, or
/// `~/.config/monad-monitor/config.toml`.
fn file_path() -> Option<PathBuf> {
    path_from(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Reads the config file, if there is one. A missing file is not an error: the
/// file is optional and the defaults stand. A file that exists but cannot be
/// read or parsed is an error, because it was written to be used.
pub fn load_file() -> Result<Layer, String> {
    let Some(path) = file_path() else {
        return Ok(Layer::default());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Layer::default()),
        Err(e) => return Err(format!("cannot read {}: {}", path.display(), e)),
    };
    parse_file(&text, &path.display().to_string())
}

/// Parses the file. Split from reading it so the format is covered by tests
/// without touching a filesystem.
///
/// The format is a flat `key = value` list, the subset of TOML this needs: `#`
/// starts a comment, a value may be quoted, and an unknown key is an error
/// rather than a line that looks applied and is not.
pub fn parse_file(text: &str, path: &str) -> Result<Layer, String> {
    let mut layer = Layer::default();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = format!("{} line {}", path, index + 1);
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{}: expected key = value, got {:?}", at, line));
        };
        let key = key.trim();
        if !KEYS.contains(&key) {
            return Err(format!(
                "{}: unknown key {:?}. Keys: {}",
                at,
                key,
                KEYS.join(", ")
            ));
        }
        layer.set(key, unquote(value.trim()), &format!("{} in {}", key, at))?;
    }
    Ok(layer)
}

/// Takes a value as written. A quoted value is used verbatim, which is how a
/// value containing a `#` gets through; a bare value ends where a trailing
/// comment starts.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    match value.split_once(" #") {
        Some((before, _)) => before.trim_end(),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(pairs: &[(&str, &str)]) -> Layer {
        let mut layer = Layer::default();
        for (flag, value) in pairs {
            layer.apply_flag(flag, value).unwrap();
        }
        layer
    }

    #[test]
    fn no_flags_and_no_file_keeps_what_used_to_be_hardcoded() {
        // These were the constants in main.rs. Anything else here is a
        // behaviour change for everyone already running the monitor.
        let cfg = Config::resolve(Layer::default(), Layer::default());
        assert_eq!(cfg.metrics_url, "http://localhost:8889/metrics");
        assert_eq!(cfg.ws_url, "ws://localhost:8081");
        assert_eq!(cfg.refresh_secs, 1);
        assert_eq!(cfg.network, "mainnet");
        assert_eq!(
            cfg.resolved_external_rpc_url(),
            "wss://rpc-mainnet.monadinfra.com"
        );
    }

    #[test]
    fn the_acceptance_example_points_at_a_remote_node() {
        let cfg = Config::resolve(
            Layer::default(),
            flags(&[
                ("--metrics-url", "http://node:8889/metrics"),
                ("--ws-url", "ws://node:8081"),
            ]),
        );
        assert_eq!(cfg.metrics_url, "http://node:8889/metrics");
        assert_eq!(cfg.ws_url, "ws://node:8081");
        // Untouched by those two flags.
        assert_eq!(cfg.refresh_secs, 1);
        assert_eq!(cfg.network, "mainnet");
    }

    #[test]
    fn a_flag_beats_the_file_and_the_file_beats_the_default() {
        let file = parse_file(
            "metrics_url = \"http://file:8889/metrics\"\nrefresh = 5\n",
            "config.toml",
        )
        .unwrap();

        let cfg = Config::resolve(file.clone(), Layer::default());
        assert_eq!(cfg.metrics_url, "http://file:8889/metrics");
        assert_eq!(cfg.refresh_secs, 5);

        let cfg = Config::resolve(
            file,
            flags(&[("--metrics-url", "http://flag:8889/metrics")]),
        );
        assert_eq!(cfg.metrics_url, "http://flag:8889/metrics");
        // The file still owns what no flag mentioned.
        assert_eq!(cfg.refresh_secs, 5);
    }

    #[test]
    fn an_explicit_external_rpc_url_wins_over_the_network_name() {
        let cfg = Config::resolve(Layer::default(), flags(&[("--network", "testnet")]));
        assert_eq!(
            cfg.resolved_external_rpc_url(),
            "wss://rpc-testnet.monadinfra.com"
        );

        let cfg = Config::resolve(
            Layer::default(),
            flags(&[
                ("--network", "testnet"),
                ("--external-rpc-url", "wss://rpc.example.com"),
            ]),
        );
        assert_eq!(cfg.resolved_external_rpc_url(), "wss://rpc.example.com");
        // The name still labels the run, so the JSON snapshot stays honest.
        assert_eq!(cfg.network, "testnet");
    }

    #[test]
    fn the_file_takes_comments_blank_lines_quoted_and_bare_values() {
        let text = "# a comment\n\nmetrics_url = http://a:8889/metrics\n\
                    ws_url = \"ws://a:8081\"\nrefresh = 3   # every three seconds\n\
                    network = testnet\n";
        let cfg = Config::resolve(parse_file(text, "config.toml").unwrap(), Layer::default());
        assert_eq!(cfg.metrics_url, "http://a:8889/metrics");
        assert_eq!(cfg.ws_url, "ws://a:8081");
        assert_eq!(cfg.refresh_secs, 3);
        assert_eq!(cfg.network, "testnet");
    }

    #[test]
    fn an_unknown_key_names_the_line_and_the_keys_that_exist() {
        // A quietly ignored typo is the worst case: the operator believes they
        // pointed the monitor at another host and it is still on localhost.
        let err = parse_file("ws_ur1 = ws://node:8081\n", "config.toml").unwrap_err();
        assert!(err.contains("line 1"), "{}", err);
        assert!(err.contains("ws_ur1"), "{}", err);
        assert!(err.contains("ws_url"), "{}", err);
    }

    #[test]
    fn a_malformed_line_names_its_number() {
        let err = parse_file("refresh = 5\nnot a setting\n", "config.toml").unwrap_err();
        assert!(err.contains("line 2"), "{}", err);
    }

    #[test]
    fn a_bad_value_is_blamed_on_where_it_came_from() {
        let err = parse_file("refresh = 0\n", "/tmp/config.toml").unwrap_err();
        assert!(err.contains("/tmp/config.toml line 1"), "{}", err);
        assert!(err.contains("refresh"), "{}", err);

        let mut layer = Layer::default();
        let err = layer.apply_flag("--refresh", "0").unwrap_err();
        assert!(err.contains("--refresh"), "{}", err);
    }

    #[test]
    fn refresh_takes_whole_seconds_inside_a_usable_range() {
        // Zero and an unbounded period both panic the timer they are handed to.
        let mut layer = Layer::default();
        assert!(layer.apply_flag("--refresh", "0").is_err());
        assert!(layer.apply_flag("--refresh", "-1").is_err());
        assert!(layer.apply_flag("--refresh", "1.5").is_err());
        assert!(layer.apply_flag("--refresh", "often").is_err());
        let too_long = (MAX_REFRESH_SECS + 1).to_string();
        assert!(layer.apply_flag("--refresh", &too_long).is_err());
        assert!(layer.apply_flag("--refresh", "1").is_ok());
        assert!(layer
            .apply_flag("--refresh", &MAX_REFRESH_SECS.to_string())
            .is_ok());
    }

    #[test]
    fn a_url_in_the_wrong_scheme_is_rejected_where_it_is_given() {
        // The mistake this catches is the metrics URL pasted into --ws-url,
        // which otherwise only shows up as a WebSocket that never connects.
        let mut layer = Layer::default();
        assert!(layer.apply_flag("--ws-url", "http://node:8081").is_err());
        assert!(layer
            .apply_flag("--metrics-url", "ws://node:8889/metrics")
            .is_err());
        assert!(layer.apply_flag("--metrics-url", "node:8889").is_err());
        assert!(layer
            .apply_flag("--external-rpc-url", "https://rpc.example.com")
            .is_err());

        assert!(layer.apply_flag("--ws-url", "wss://node/ws").is_ok());
        assert!(layer
            .apply_flag("--metrics-url", "https://node/metrics")
            .is_ok());
    }

    #[test]
    fn a_network_name_has_to_fit_in_a_hostname() {
        let mut layer = Layer::default();
        assert!(layer.apply_flag("--network", "").is_err());
        assert!(layer.apply_flag("--network", "test net").is_err());
        assert!(layer.apply_flag("--network", "../elsewhere").is_err());
        assert!(layer.apply_flag("--network", "testnet-2").is_ok());
    }

    #[test]
    fn only_endpoint_flags_are_claimed() {
        // The argument loop owns everything else; claiming a flag here would
        // swallow one this module knows nothing about.
        assert!(is_config_flag("--metrics-url"));
        assert!(is_config_flag("--external-rpc-url"));
        assert!(!is_config_flag("--json"));
        assert!(!is_config_flag("--alert-min-peers"));
        // Only the documented spelling: an underscore is a typo, not a synonym.
        assert!(!is_config_flag("--metrics_url"));
        assert!(!is_config_flag("metrics-url"));

        let mut layer = Layer::default();
        assert!(layer
            .apply_flag("--webhook-url", "https://example.com/hook")
            .is_err());
    }

    #[test]
    fn the_config_file_path_follows_xdg_then_home() {
        assert_eq!(
            path_from(Some("/x".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/x/monad-monitor/config.toml")
        );
        assert_eq!(
            path_from(None, Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u/.config/monad-monitor/config.toml")
        );
        assert_eq!(path_from(None, None), None);
    }
}
