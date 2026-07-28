mod alerts;
mod metrics;
mod rpc;
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

use crate::alerts::{AlertConfig, AlertEngine, AlertSample};
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
    WebhookError(String),
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse alert settings before touching the terminal so errors print cleanly
    let alert_config = match AlertConfig::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("Error: {}", err);
            std::process::exit(2);
        }
    };

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run app
    let result = run_app(&mut terminal, alert_config).await;

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

async fn run_app<B: Backend>(terminal: &mut Terminal<B>, alert_config: AlertConfig) -> Result<()> {
    let mut state = AppState::new();

    // Alert engine and a client for webhook deliveries
    let mut alert_engine = AlertEngine::new(alert_config);
    let webhook_client = reqwest::Client::new();

    // Alerts only look at data that has actually arrived, never at defaults
    let mut have_metrics = false;
    let mut have_system = false;

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
                    DataUpdate::Metrics(Ok(metrics)) => {
                        state.update_metrics(metrics);
                        have_metrics = true;
                    }
                    DataUpdate::Metrics(Err(e)) => state.set_error(format!("metrics: {}", e)),
                    DataUpdate::Rpc(rpc_data) => state.update_rpc(rpc_data),
                    DataUpdate::System(Ok(system)) => {
                        state.update_system(system);
                        have_system = true;
                    }
                    DataUpdate::System(Err(e)) => state.set_error(format!("system: {}", e)),
                    DataUpdate::WebhookError(e) => state.set_error(format!("webhook: {}", e)),
                }
            }

            // UI refresh tick for animations
            _ = ui_ticker.tick() => {
                // Just triggers a redraw
            }
        }

        // Check alert thresholds after every event so the block stall timer is
        // watched even when no new data arrives. Transitions only: the engine
        // reports one event when a threshold trips and one when it recovers.
        if alert_engine.enabled() {
            let sample = AlertSample {
                secs_since_last_block: state.time_since_last_block().map(|d| d.as_secs_f64()),
                finalized_lag: have_system.then(|| state.system.finalized_lag()),
                peer_count: have_metrics.then_some(state.metrics.peer_count),
                disk_used_pct: have_system.then_some(state.system.disk_used_pct),
            };
            for event in alert_engine.evaluate(&sample) {
                if let Some(url) = alert_engine.webhook_url() {
                    let client = webhook_client.clone();
                    let url = url.to_string();
                    let node = if state.system.node_id.is_empty() {
                        "unknown".to_string()
                    } else {
                        state.system.node_id.clone()
                    };
                    let tx_hook = tx.clone();
                    // Deliver in the background; a slow or dead webhook must
                    // never stall the UI. Failures surface in the footer.
                    tokio::spawn(async move {
                        if let Err(e) = alerts::post_webhook(&client, &url, &event, &node).await {
                            let _ = tx_hook.send(DataUpdate::WebhookError(e)).await;
                        }
                    });
                }
            }
            state.active_alerts = alert_engine.active();
        }
    }
}
