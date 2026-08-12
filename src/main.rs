mod alerts;
mod config;
mod metrics;
mod rpc;
mod snapshot;
mod state;
mod system;
mod ui;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::prelude::*;
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::alerts::AlertConfig;
use crate::config::Config;
use crate::metrics::{MetricsClient, PrometheusMetrics};
use crate::rpc::{RpcClient, RpcEvent};
use crate::state::AppState;
use crate::system::{SystemClient, SystemData};

const SYSTEM_REFRESH_INTERVAL_MS: u64 = 5000;
/// How often the alert thresholds are evaluated. One second keeps the confirm
/// window in the configuration readable: it is a count of seconds.
const ALERT_INTERVAL_MS: u64 = 1000;

enum DataUpdate {
    Metrics(Result<PrometheusMetrics, String>),
    Rpc(RpcEvent),
    System(Result<SystemData, String>),
    Webhook(Result<(), String>),
}

/// What the command-line arguments asked us to do.
#[derive(Debug, PartialEq)]
enum Cli {
    /// Run the interactive TUI (the default), with whatever alert thresholds
    /// and endpoints were configured.
    Tui {
        alerts: AlertConfig,
        endpoints: config::Layer,
    },
    /// Print JSON snapshots. `watch` is the interval in seconds for streaming
    /// NDJSON, or `None` for a single snapshot.
    Json {
        watch: Option<u64>,
        endpoints: config::Layer,
    },
    /// Print usage and exit.
    Help,
}

fn parse_args(args: &[String]) -> std::result::Result<Cli, String> {
    let mut json = false;
    let mut watch: Option<u64> = None;
    let mut alerts = AlertConfig::default();
    let mut endpoints = config::Layer::default();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--watch" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<u64>().ok()) {
                    Some(secs) if secs > 0 => watch = Some(secs),
                    _ => {
                        return Err(
                            "--watch requires a positive integer number of seconds".to_string()
                        )
                    }
                }
            }
            "-h" | "--help" => return Ok(Cli::Help),
            flag if alerts::is_alert_flag(flag) => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| format!("{} requires a value", flag))?;
                alerts.apply(flag, value)?;
            }
            flag if config::is_config_flag(flag) => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| format!("{} requires a value", flag))?;
                endpoints.apply_flag(flag, value)?;
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
        i += 1;
    }

    if watch.is_some() && !json {
        return Err("--watch only applies to --json mode".to_string());
    }

    // The snapshot prints one reading and exits, so it has no state in which to
    // raise or clear an alert. Saying so beats accepting the flag and ignoring
    // it.
    if json && alerts.is_configured() {
        return Err("alert flags only apply to the TUI, not to --json".to_string());
    }

    // The snapshot has its own interval flag. Two names for one interval would
    // be worse than saying which one this mode reads.
    if json && endpoints.sets_refresh() {
        return Err(
            "--refresh applies to the TUI; --json --watch <secs> sets the snapshot interval"
                .to_string(),
        );
    }

    if json {
        Ok(Cli::Json { watch, endpoints })
    } else {
        Ok(Cli::Tui { alerts, endpoints })
    }
}

fn print_help() {
    println!("monad-monitor - terminal monitor for a Monad node");
    println!();
    println!("USAGE:");
    println!("    monad-monitor                        Run the interactive TUI (default)");
    println!("    monad-monitor --json                 Print one JSON snapshot and exit");
    println!("    monad-monitor --json --watch <secs>  Print a JSON object every <secs> (NDJSON)");
    println!("    monad-monitor --help                 Show this help");
    println!();
    println!("ALERTS (TUI only, off unless configured):");
    println!("    --webhook-url <url>          POST a JSON payload here on each transition");
    println!("    --alert-no-block <secs>      No new block over the node's WebSocket");
    println!("    --alert-finalized-lag <n>    Finalized lag past n blocks");
    println!("    --alert-min-peers <n>        Peer count below n");
    println!("    --alert-disk <pct>           Disk usage above pct");
    println!("    --alert-confirm <secs>       Hold time before a change counts (default 3)");
    println!("    --alert-cooldown <secs>      Gap between alerts for one threshold (default 300)");
    println!();
    println!("ENDPOINTS (defaults are the values that used to be hardcoded):");
    println!("    --metrics-url <url>          Prometheus endpoint the monitor scrapes");
    println!("                                 (default http://localhost:8889/metrics)");
    println!("    --ws-url <url>               The node's WebSocket, for real-time blocks");
    println!("                                 (default ws://localhost:8081)");
    println!("    --refresh <secs>             Metrics scrape interval, TUI only (default 1)");
    println!("    --network <name>             Network whose public node the block height is");
    println!("                                 compared against (default mainnet)");
    println!("    --external-rpc-url <url>     That comparison endpoint, given outright");
    println!();
    println!("CONFIG FILE (optional):");
    println!("    ~/.config/monad-monitor/config.toml, or the same path under");
    println!("    $XDG_CONFIG_HOME. Keys are the endpoint flags with underscores:");
    println!("    metrics_url, ws_url, refresh, network, external_rpc_url. A flag beats");
    println!("    the file, and the file beats the default.");
    println!();
    println!("The headless snapshot reuses the same data path as the TUI (metrics, RPC and");
    println!("system stats). In one-shot mode the exit status is non-zero when the node is");
    println!("unreachable, so a shell or cron check can alert on it.");
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("{}", msg);
            print_help();
            std::process::exit(2);
        }
    };

    match cli {
        Cli::Help => {
            print_help();
            Ok(())
        }
        Cli::Json { watch, endpoints } => {
            let code = snapshot::run(&settings(endpoints), watch).await?;
            std::process::exit(code);
        }
        Cli::Tui { alerts, endpoints } => run_tui(settings(endpoints), alerts).await,
    }
}

/// Merges the config file under the flags. A file that exists and does not
/// parse stops the run: carrying on with the defaults would monitor localhost
/// while the operator believes they configured a remote node.
fn settings(endpoints: config::Layer) -> Config {
    match config::load_file() {
        Ok(file) => Config::resolve(file, endpoints),
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(2);
        }
    }
}

async fn run_tui(cfg: Config, alert_config: AlertConfig) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run app
    let result = run_app(&mut terminal, cfg, alert_config).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        eprintln!("Error: {}", err);
    }

    Ok(())
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    cfg: Config,
    alert_config: AlertConfig,
) -> Result<()> {
    let mut state = AppState::new();

    // One client for every webhook POST, and only when a webhook is configured.
    let webhook_client = alert_config
        .webhook_url
        .as_ref()
        .map(|_| reqwest::Client::new());

    // Channel for receiving data updates from background tasks
    let (tx, mut rx) = mpsc::channel::<DataUpdate>(100);

    // Spawn RPC subscription (real-time block updates)
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcEvent>(100);
    let rpc_client = RpcClient::new(&cfg.ws_url);
    rpc_client.subscribe(rpc_tx);

    // Forward RPC updates to main channel
    let tx_rpc = tx.clone();
    tokio::spawn(async move {
        while let Some(event) = rpc_rx.recv().await {
            let _ = tx_rpc.send(DataUpdate::Rpc(event)).await;
        }
    });

    // Spawn background data fetcher for metrics (polling)
    let tx_metrics = tx.clone();
    let metrics_url = cfg.metrics_url.clone();
    let metrics_period = Duration::from_secs(cfg.refresh_secs);
    tokio::spawn(async move {
        let metrics_client = MetricsClient::new(&metrics_url);
        let mut refresh_interval = interval(metrics_period);

        loop {
            refresh_interval.tick().await;
            // The error names the endpoint: with the URL configurable,
            // "Failed to fetch metrics" no longer says which one went quiet.
            let result = metrics_client
                .fetch()
                .await
                .map_err(|e| format!("{} ({})", e, metrics_client.endpoint()));
            let _ = tx_metrics.send(DataUpdate::Metrics(result)).await;
        }
    });

    // Spawn background data fetcher for system data (less frequent)
    let tx_system = tx.clone();
    let external_rpc_url = cfg.resolved_external_rpc_url();
    tokio::spawn(async move {
        let mut system_client = SystemClient::new(&external_rpc_url);
        let mut refresh_interval = interval(Duration::from_millis(SYSTEM_REFRESH_INTERVAL_MS));

        loop {
            refresh_interval.tick().await;
            let system_result = system_client.fetch().await;
            let _ = tx_system
                .send(DataUpdate::System(system_result.map_err(|e| e.to_string())))
                .await;
        }
    });

    // Create async event stream for keyboard
    let mut event_stream = crossterm::event::EventStream::new();

    // UI refresh ticker for smooth animations (100ms = 10fps)
    let mut ui_ticker = interval(Duration::from_millis(100));

    // Alert evaluation runs on its own beat, not on the draw loop: thresholds
    // are about how long something has been true, not how often the screen
    // redraws.
    let mut alert_ticker = interval(Duration::from_millis(ALERT_INTERVAL_MS));
    // Monotonic reference for the alert cooldown; only differences matter.
    let started = std::time::Instant::now();

    loop {
        // Draw UI
        terminal.draw(|frame| ui::draw(frame, &state))?;

        // Wait for keyboard input, data update, or UI tick
        tokio::select! {
            // Handle keyboard events (highest priority)
            maybe_event = event_stream.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                                return Ok(());
                            }
                            KeyCode::Char('t') | KeyCode::Char('T') => {
                                state.toggle_theme();
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Handle data updates from background tasks
            Some(update) = rx.recv() => {
                match update {
                    DataUpdate::Metrics(Ok(metrics)) => state.update_metrics(metrics),
                    DataUpdate::Metrics(Err(e)) => state.set_error(format!("metrics: {}", e)),
                    DataUpdate::Rpc(RpcEvent::Data(rpc_data)) => state.update_rpc(rpc_data),
                    DataUpdate::Rpc(RpcEvent::Connected) => state.set_ws_connected(),
                    DataUpdate::Rpc(RpcEvent::Disconnected(reason)) => state.set_ws_disconnected(reason),
                    DataUpdate::System(Ok(system)) => state.update_system(system),
                    DataUpdate::System(Err(e)) => state.set_error(format!("system: {}", e)),
                    DataUpdate::Webhook(result) => state.set_webhook_result(result),
                }
            }

            // Threshold evaluation
            _ = alert_ticker.tick() => {
                if alert_config.is_enabled() {
                    let sample = state.alert_sample();
                    let now = started.elapsed().as_secs();
                    let transitions = alerts::evaluate(&mut state.alerts, &alert_config, &sample, now);

                    if let (Some(client), Some(url)) = (&webhook_client, &alert_config.webhook_url) {
                        let node = state.system.node_id.clone();
                        for transition in transitions {
                            // Spawned, so a slow webhook cannot delay the next
                            // frame or the next evaluation. The outcome comes
                            // back through the same channel as every other
                            // source, so a webhook that stops delivering says
                            // so on screen.
                            let client = client.clone();
                            let url = url.clone();
                            let body = transition.payload(&node);
                            let tx_webhook = tx.clone();
                            tokio::spawn(async move {
                                let result = alerts::post(client, url, body).await;
                                let _ = tx_webhook.send(DataUpdate::Webhook(result)).await;
                            });
                        }
                    }
                }
            }

            // UI refresh tick for animations
            _ = ui_ticker.tick() => {
                // Just triggers a redraw
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_runs_the_tui() {
        assert_eq!(
            parse_args(&args(&[])).unwrap(),
            Cli::Tui {
                alerts: AlertConfig::default(),
                endpoints: config::Layer::default(),
            }
        );
    }

    #[test]
    fn alert_flags_configure_the_tui() {
        let cli = parse_args(&args(&[
            "--alert-min-peers",
            "10",
            "--webhook-url",
            "https://example.com/hook",
        ]))
        .unwrap();

        match cli {
            Cli::Tui { alerts, .. } => {
                assert_eq!(alerts.min_peers, Some(10));
                assert_eq!(alerts.webhook_url.as_deref(), Some("https://example.com/hook"));
                assert!(alerts.is_configured());
            }
            other => panic!("expected the TUI, got {:?}", other),
        }
    }

    #[test]
    fn a_mistyped_alert_flag_is_rejected_rather_than_ignored() {
        // Silently skipping this would leave the operator believing a threshold
        // is armed when nothing is watching it.
        assert!(parse_args(&args(&["--alert-noblock", "30"])).is_err());
        assert!(parse_args(&args(&["--alert-min-peers"])).is_err());
        assert!(parse_args(&args(&["--alert-min-peers", "lots"])).is_err());
    }

    #[test]
    fn alert_flags_are_rejected_in_json_mode() {
        assert!(parse_args(&args(&["--json", "--alert-min-peers", "10"])).is_err());
    }

    #[test]
    fn json_flag_selects_snapshot() {
        assert_eq!(
            parse_args(&args(&["--json"])).unwrap(),
            Cli::Json {
                watch: None,
                endpoints: config::Layer::default(),
            }
        );
    }

    #[test]
    fn json_with_watch_sets_interval() {
        assert_eq!(
            parse_args(&args(&["--json", "--watch", "5"])).unwrap(),
            Cli::Json {
                watch: Some(5),
                endpoints: config::Layer::default(),
            }
        );
    }

    #[test]
    fn help_is_recognized() {
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), Cli::Help);
        assert_eq!(parse_args(&args(&["-h"])).unwrap(), Cli::Help);
    }

    #[test]
    fn watch_without_json_is_rejected() {
        assert!(parse_args(&args(&["--watch", "5"])).is_err());
    }

    #[test]
    fn watch_requires_a_positive_integer() {
        assert!(parse_args(&args(&["--json", "--watch", "0"])).is_err());
        assert!(parse_args(&args(&["--json", "--watch", "abc"])).is_err());
        assert!(parse_args(&args(&["--json", "--watch"])).is_err());
    }

    #[test]
    fn unknown_argument_is_rejected() {
        assert!(parse_args(&args(&["--nope"])).is_err());
    }

    #[test]
    fn endpoint_flags_are_read_in_both_modes() {
        let cli = parse_args(&args(&[
            "--metrics-url",
            "http://node:8889/metrics",
            "--ws-url",
            "ws://node:8081",
        ]))
        .unwrap();

        let endpoints = match cli {
            Cli::Tui { endpoints, .. } => endpoints,
            other => panic!("expected the TUI, got {:?}", other),
        };
        let cfg = Config::resolve(config::Layer::default(), endpoints);
        assert_eq!(cfg.metrics_url, "http://node:8889/metrics");
        assert_eq!(cfg.ws_url, "ws://node:8081");

        // The snapshot reads the same endpoints, so pointing --json at a
        // remote node does not need a second set of flags.
        assert!(parse_args(&args(&["--json", "--ws-url", "ws://node:8081"])).is_ok());
    }

    #[test]
    fn a_mistyped_endpoint_flag_is_rejected_rather_than_ignored() {
        // Accepting these quietly would leave the monitor on localhost while
        // the operator believes it is watching another host.
        assert!(parse_args(&args(&["--metrics_url", "http://node:8889/metrics"])).is_err());
        assert!(parse_args(&args(&["--metrics-url"])).is_err());
        assert!(parse_args(&args(&["--ws-url", "http://node:8081"])).is_err());
    }

    #[test]
    fn refresh_is_rejected_in_snapshot_mode() {
        assert!(parse_args(&args(&["--refresh", "5"])).is_ok());
        // --watch is the snapshot's interval, so taking --refresh here and
        // ignoring it would look like it had been set.
        assert!(parse_args(&args(&["--json", "--refresh", "5"])).is_err());
    }
}
