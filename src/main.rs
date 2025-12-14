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

use crate::metrics::{MetricsClient, PrometheusMetrics};
use crate::rpc::{RpcClient, RpcData};
use crate::state::AppState;
use crate::system::{SystemClient, SystemData};

const METRICS_ENDPOINT: &str = "http://localhost:8889/metrics";
const RPC_ENDPOINT: &str = "http://localhost:8080";
const NETWORK: &str = "mainnet";
const REFRESH_INTERVAL_MS: u64 = 1000;
const SYSTEM_REFRESH_INTERVAL_MS: u64 = 5000; // System data refreshes less frequently

enum DataUpdate {
    Metrics(Result<PrometheusMetrics, String>),
    Rpc(Result<RpcData, String>),
    System(Result<SystemData, String>),
}

#[tokio::main]
async fn main() -> Result<()> {
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
    let (tx, mut rx) = mpsc::channel::<DataUpdate>(10);

    // Spawn background data fetcher for metrics and RPC
    let tx_clone = tx.clone();
    tokio::spawn(async move {
        let metrics_client = MetricsClient::new(METRICS_ENDPOINT);
        let rpc_client = RpcClient::new(RPC_ENDPOINT);
        let mut refresh_interval = interval(Duration::from_millis(REFRESH_INTERVAL_MS));

        loop {
            refresh_interval.tick().await;

            // Fetch both in parallel
            let (metrics_result, rpc_result) = tokio::join!(
                metrics_client.fetch(),
                rpc_client.fetch()
            );

            let _ = tx_clone.send(DataUpdate::Metrics(
                metrics_result.map_err(|e| e.to_string())
            )).await;

            let _ = tx_clone.send(DataUpdate::Rpc(
                rpc_result.map_err(|e| e.to_string())
            )).await;
        }
    });

    // Spawn background data fetcher for system data (less frequent)
    let tx_clone = tx.clone();
    tokio::spawn(async move {
        let system_client = SystemClient::new(NETWORK);
        let mut refresh_interval = interval(Duration::from_millis(SYSTEM_REFRESH_INTERVAL_MS));

        loop {
            refresh_interval.tick().await;

            let system_result = system_client.fetch().await;

            let _ = tx_clone.send(DataUpdate::System(
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
                            _ => {}
                        }
                    }
                }
            }

            // Handle data updates from background task
            Some(update) = rx.recv() => {
                match update {
                    DataUpdate::Metrics(Ok(metrics)) => state.update_metrics(metrics),
                    DataUpdate::Metrics(Err(e)) => state.set_error(format!("metrics: {}", e)),
                    DataUpdate::Rpc(Ok(rpc_data)) => state.update_rpc(rpc_data),
                    DataUpdate::Rpc(Err(e)) => state.set_error(format!("rpc: {}", e)),
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
