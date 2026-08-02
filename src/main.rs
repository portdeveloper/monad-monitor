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

use crate::metrics::{MetricsClient, PrometheusMetrics};
use crate::rpc::{RpcClient, RpcData};
use crate::state::AppState;
use crate::system::{SystemClient, SystemData};

const METRICS_ENDPOINT: &str = "http://localhost:8889/metrics";
const RPC_ENDPOINT: &str = "ws://localhost:8081";
const NETWORK: &str = "mainnet";
const METRICS_REFRESH_INTERVAL_MS: u64 = 1000;
const SYSTEM_REFRESH_INTERVAL_MS: u64 = 5000;

enum DataUpdate {
    Metrics(Result<PrometheusMetrics, String>),
    Rpc(RpcData),
    System(Result<SystemData, String>),
}

/// What the command-line arguments asked us to do.
#[derive(Debug, PartialEq)]
enum Cli {
    /// Run the interactive TUI (the default).
    Tui,
    /// Print JSON snapshots. `watch` is the interval in seconds for streaming
    /// NDJSON, or `None` for a single snapshot.
    Json { watch: Option<u64> },
    /// Print usage and exit.
    Help,
}

fn parse_args(args: &[String]) -> std::result::Result<Cli, String> {
    let mut json = false;
    let mut watch: Option<u64> = None;

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
            other => return Err(format!("unknown argument: {}", other)),
        }
        i += 1;
    }

    if watch.is_some() && !json {
        return Err("--watch only applies to --json mode".to_string());
    }

    if json {
        Ok(Cli::Json { watch })
    } else {
        Ok(Cli::Tui)
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
        Cli::Json { watch } => {
            let code = snapshot::run(NETWORK, METRICS_ENDPOINT, RPC_ENDPOINT, watch).await?;
            std::process::exit(code);
        }
        Cli::Tui => run_tui().await,
    }
}

async fn run_tui() -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run app
    let result = run_app(&mut terminal).await;

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

async fn run_app<B: Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    let mut state = AppState::new();

    // Channel for receiving data updates from background tasks
    let (tx, mut rx) = mpsc::channel::<DataUpdate>(100);

    // Spawn RPC subscription (real-time block updates)
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcData>(100);
    let rpc_client = RpcClient::new(RPC_ENDPOINT);
    rpc_client.subscribe(rpc_tx);

    // Forward RPC updates to main channel
    let tx_rpc = tx.clone();
    tokio::spawn(async move {
        while let Some(rpc_data) = rpc_rx.recv().await {
            let _ = tx_rpc.send(DataUpdate::Rpc(rpc_data)).await;
        }
    });

    // Spawn background data fetcher for metrics (polling)
    let tx_metrics = tx.clone();
    tokio::spawn(async move {
        let metrics_client = MetricsClient::new(METRICS_ENDPOINT);
        let mut refresh_interval = interval(Duration::from_millis(METRICS_REFRESH_INTERVAL_MS));

        loop {
            refresh_interval.tick().await;
            let metrics_result = metrics_client.fetch().await;
            let _ = tx_metrics.send(DataUpdate::Metrics(
                metrics_result.map_err(|e| e.to_string())
            )).await;
        }
    });

    // Spawn background data fetcher for system data (less frequent)
    let tx_system = tx.clone();
    tokio::spawn(async move {
        let system_client = SystemClient::new(NETWORK);
        let mut refresh_interval = interval(Duration::from_millis(SYSTEM_REFRESH_INTERVAL_MS));

        loop {
            refresh_interval.tick().await;
            let system_result = system_client.fetch().await;
            let _ = tx_system.send(DataUpdate::System(
                system_result.map_err(|e| e.to_string())
            )).await;
        }
    });

    // Create async event stream for keyboard
    let mut event_stream = crossterm::event::EventStream::new();

    // UI refresh ticker for smooth animations (100ms = 10fps)
    let mut ui_ticker = interval(Duration::from_millis(100));

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
                    DataUpdate::Rpc(rpc_data) => state.update_rpc(rpc_data),
                    DataUpdate::System(Ok(system)) => state.update_system(system),
                    DataUpdate::System(Err(e)) => state.set_error(format!("system: {}", e)),
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
        assert_eq!(parse_args(&args(&[])).unwrap(), Cli::Tui);
    }

    #[test]
    fn json_flag_selects_snapshot() {
        assert_eq!(
            parse_args(&args(&["--json"])).unwrap(),
            Cli::Json { watch: None }
        );
    }

    #[test]
    fn json_with_watch_sets_interval() {
        assert_eq!(
            parse_args(&args(&["--json", "--watch", "5"])).unwrap(),
            Cli::Json { watch: Some(5) }
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
}
