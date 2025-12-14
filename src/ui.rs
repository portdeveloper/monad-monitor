use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Sparkline, Table},
    Frame,
};

use crate::state::AppState;

// Monad brand colors
const MONAD_PRIMARY: Color = Color::Rgb(110, 84, 255);  // #6E54FF
const MONAD_LIGHT: Color = Color::Rgb(221, 215, 254);   // #DDD7FE
const MONAD_DARK: Color = Color::Rgb(14, 9, 28);        // #0E091C

const TITLE_COLOR: Color = MONAD_PRIMARY;               // Titles in brand purple
const LABEL_COLOR: Color = Color::Rgb(160, 160, 160);   // Labels and borders
const VALUE_COLOR: Color = Color::Rgb(220, 220, 220);   // Values - bright
const TEXT_DIM: Color = Color::Rgb(180, 180, 180);      // Dimmed data text

pub fn draw(frame: &mut Frame, state: &AppState) {
    let area = frame.area();

    // Main layout: header, secondary stats, sparkline, blocks, footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(5),  // Header stats (block, peers, tps, latency)
            Constraint::Length(3),  // Secondary stats (disk, services, diff, epoch)
            Constraint::Length(5),  // TPS sparkline
            Constraint::Min(6),     // Recent blocks
            Constraint::Length(3),  // Footer
        ])
        .split(area);

    draw_header(frame, chunks[0], state);
    draw_secondary_stats(frame, chunks[1], state);
    draw_sparkline(frame, chunks[2], state);
    draw_blocks(frame, chunks[3], state);
    draw_footer(frame, chunks[4], state);
}

fn draw_header(frame: &mut Frame, area: Rect, state: &AppState) {
    // Pulsing heartbeat - smooth color fade from brand purple to light
    let pulse = state.pulse_intensity();

    // Fade from #6E54FF (bright) to #DDD7FE (dim/idle)
    let pulse_color = Color::Rgb(
        (221.0 - 111.0 * pulse) as u8,  // R: 221 -> 110
        (215.0 - 131.0 * pulse) as u8,  // G: 215 -> 84
        (254.0 + 1.0 * pulse) as u8,    // B: 254 -> 255
    );

    let title = Line::from(vec![
        Span::styled(" monad-monitor ", Style::default().fg(TITLE_COLOR).bold()),
        Span::styled("●", Style::default().fg(pulse_color)),
        Span::raw(" "),
    ]);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(LABEL_COLOR));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Four columns: Block Height | Peers | TPS | Latency
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(inner);

    // Block height with block difference
    let block_num = state.block_height();
    let sync_status = state.sync_status();
    let block_diff = state.system.block_difference(block_num);
    let sync_color = if sync_status == "synced" && block_diff.abs() < 5 {
        Color::Green
    } else if block_diff.abs() < 20 {
        Color::Yellow
    } else {
        Color::Red
    };

    let diff_str = if block_diff == 0 {
        "Δ0".to_string()
    } else if block_diff > 0 {
        format!("Δ-{}", block_diff)
    } else {
        format!("Δ+{}", block_diff.abs())
    };

    let block_text = vec![
        Line::from(Span::styled("BLOCK HEIGHT", Style::default().fg(LABEL_COLOR))),
        Line::from(Span::styled(
            format_number(block_num),
            Style::default().fg(VALUE_COLOR).bold(),
        )),
        Line::from(vec![
            Span::styled("✓ ", Style::default().fg(sync_color)),
            Span::styled(sync_status, Style::default().fg(sync_color)),
            Span::styled(format!(" ({})", diff_str), Style::default().fg(LABEL_COLOR)),
        ]),
    ];
    frame.render_widget(Paragraph::new(block_text).alignment(Alignment::Center), columns[0]);

    // Peers with trend
    let peer_count = state.metrics.peer_count;
    let peer_health = state.peer_health();
    let peers_trend = state.peers_trend();
    let peer_color = match peer_health {
        "healthy" => Color::Green,
        "ok" => Color::Yellow,
        _ => Color::Red,
    };

    let (peer_trend_arrow, peer_trend_color) = match peers_trend {
        1 => ("▲", Color::Green),   // More peers = good
        -1 => ("▼", Color::Red),    // Fewer peers = bad
        _ => ("", LABEL_COLOR),
    };

    let peer_text = vec![
        Line::from(Span::styled("PEERS", Style::default().fg(LABEL_COLOR))),
        Line::from(vec![
            Span::styled(format!("{}", peer_count), Style::default().fg(VALUE_COLOR).bold()),
            Span::styled(format!(" {}", peer_trend_arrow), Style::default().fg(peer_trend_color)),
        ]),
        Line::from(vec![
            Span::styled("↑ ", Style::default().fg(peer_color)),
            Span::styled(peer_health, Style::default().fg(peer_color)),
        ]),
    ];
    frame.render_widget(Paragraph::new(peer_text).alignment(Alignment::Center), columns[1]);

    // TPS with peak and trend
    let tps = state.tps;
    let tps_peak = state.tps_peak;
    let tps_trend = state.tps_trend();

    let (trend_arrow, trend_color) = match tps_trend {
        1 => ("▲", Color::Green),
        -1 => ("▼", Color::Red),
        _ => ("", LABEL_COLOR),
    };

    let tps_text = vec![
        Line::from(Span::styled("TPS", Style::default().fg(LABEL_COLOR))),
        Line::from(vec![
            Span::styled(format!("{:.0}", tps), Style::default().fg(MONAD_PRIMARY).bold()),
            Span::styled(format!(" {}", trend_arrow), Style::default().fg(trend_color)),
        ]),
        Line::from(Span::styled(format!("peak: {:.0}", tps_peak), Style::default().fg(LABEL_COLOR))),
    ];
    frame.render_widget(Paragraph::new(tps_text).alignment(Alignment::Center), columns[2]);

    // Latency (p99) with trend
    let latency = state.metrics.latency_p99_ms;
    let latency_trend = state.latency_trend();
    let latency_color = if latency < 100 {
        Color::Green
    } else if latency < 500 {
        Color::Yellow
    } else {
        Color::Red
    };

    // For latency: up arrow = bad (red), down arrow = good (green)
    let (trend_arrow, trend_color) = match latency_trend {
        1 => ("▲", Color::Red),    // Latency increasing = bad
        -1 => ("▼", Color::Green), // Latency decreasing = good
        _ => ("", LABEL_COLOR),
    };

    let latency_text = vec![
        Line::from(Span::styled("LATENCY", Style::default().fg(LABEL_COLOR))),
        Line::from(vec![
            Span::styled(format!("{}ms", latency), Style::default().fg(latency_color).bold()),
            Span::styled(format!(" {}", trend_arrow), Style::default().fg(trend_color)),
        ]),
        Line::from(Span::styled("p99", Style::default().fg(LABEL_COLOR))),
    ];
    frame.render_widget(Paragraph::new(latency_text).alignment(Alignment::Center), columns[3]);
}

fn draw_secondary_stats(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(LABEL_COLOR));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Build stats line
    let sys = &state.system;

    // Disk usage
    let disk_color = if sys.disk_used_pct < 50.0 {
        Color::Green
    } else if sys.disk_used_pct < 80.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    // Services status
    let services_ok = sys.all_services_running();
    let services_color = if services_ok { Color::Green } else { Color::Red };
    let services_str = if services_ok { "✓ all" } else { "✗ down" };

    // Finalized lag
    let fin_lag = sys.finalized_lag();
    let ver_lag = sys.verified_lag();
    let lag_color = if fin_lag <= 3 { Color::Green } else if fin_lag <= 10 { Color::Yellow } else { Color::Red };

    // History info
    let history_str = format!("{} blocks", format_number(sys.history_count));

    let stats = Line::from(vec![
        Span::styled("DISK: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(format!("{:.1}%", sys.disk_used_pct), Style::default().fg(disk_color)),
        Span::styled(format!(" ({:.0}GB)", sys.disk_used_gb), Style::default().fg(LABEL_COLOR)),
        Span::raw("  |  "),
        Span::styled("SERVICES: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(services_str, Style::default().fg(services_color)),
        Span::raw("  |  "),
        Span::styled("FINALIZED: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(format!("-{}", fin_lag), Style::default().fg(lag_color)),
        Span::styled(format!(" (ver -{})", ver_lag), Style::default().fg(LABEL_COLOR)),
        Span::raw("  |  "),
        Span::styled("HISTORY: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(history_str, Style::default().fg(VALUE_COLOR)),
    ]);

    frame.render_widget(Paragraph::new(stats), inner);
}

fn draw_sparkline(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .title(" TPS ")
        .title_style(Style::default().fg(LABEL_COLOR))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(LABEL_COLOR));

    // Calculate available width (subtract 2 for borders)
    let available_width = area.width.saturating_sub(2) as usize;

    // Get data and pad left with zeros to fill width (right-align the graph)
    let raw_data = state.tps_sparkline_data();
    let raw_len = raw_data.len();
    let data: Vec<u64> = if raw_len < available_width {
        let padding = available_width - raw_len;
        std::iter::repeat(0).take(padding).chain(raw_data).collect()
    } else {
        raw_data.into_iter().skip(raw_len - available_width).collect()
    };

    let sparkline = Sparkline::default()
        .block(block)
        .data(&data)
        .style(Style::default().fg(MONAD_PRIMARY))
        .bar_set(symbols::bar::NINE_LEVELS);

    frame.render_widget(sparkline, area);
}

fn draw_blocks(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .title(" RECENT BLOCKS ")
        .title_style(Style::default().fg(LABEL_COLOR))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(LABEL_COLOR));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Calculate how many rows we can show (subtract 1 for header)
    let available_rows = inner.height.saturating_sub(1) as usize;

    // Determine if we have room for full hashes (need ~100 chars width)
    let wide_mode = inner.width >= 100;
    let hash_width: u16 = if wide_mode { 66 } else { 16 }; // Full hash is 66 chars

    let all_blocks = state.recent_blocks();
    let blocks_to_show = &all_blocks[..all_blocks.len().min(available_rows)];

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let rows: Vec<Row> = blocks_to_show
        .iter()
        .map(|b| {
            let hash_display = if wide_mode {
                b.hash.clone()
            } else if b.hash.len() > 14 {
                format!("{}...{}", &b.hash[..8], &b.hash[b.hash.len() - 4..])
            } else {
                b.hash.clone()
            };

            let age = if b.timestamp > 0 && now_ts >= b.timestamp {
                let secs = now_ts - b.timestamp;
                format!("{}s ago", secs)
            } else {
                "...".to_string()
            };

            let gas_pct = if b.gas_limit > 0 {
                (b.gas_used as f64 / b.gas_limit as f64) * 100.0
            } else {
                0.0
            };

            // Gas bar with percentage overlay: "███47%░░░"
            let pct_str = format!("{:.0}%", gas_pct);
            let bar_total = 9; // Total width
            let pct_len = pct_str.len();
            let bar_space = bar_total - pct_len; // Space for bar chars
            let filled = ((gas_pct / 100.0) * bar_space as f64).round() as usize;
            let empty = bar_space.saturating_sub(filled);
            let gas_bar = format!("{}{}{}", "█".repeat(filled), pct_str, "░".repeat(empty));

            Row::new(vec![
                format!("#{}", format_number(b.number)),
                format!("{} txs", b.tx_count),
                hash_display,
                gas_bar,
                age,
            ])
            .style(Style::default().fg(TEXT_DIM))
        })
        .collect();

    let widths = [
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(hash_width),
        Constraint::Length(9),  // Gas bar with % overlay
        Constraint::Length(10),
    ];

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["BLOCK", "TXS", "HASH", "GAS", "AGE"])
                .style(Style::default().fg(LABEL_COLOR).add_modifier(Modifier::BOLD)),
        )
        .column_spacing(2);

    frame.render_widget(table, inner);
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(LABEL_COLOR));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Uptime
    let uptime = state.metrics.uptime_human();

    // Gas price
    let gas_gwei = state.rpc_data.gas_price_gwei;

    // Client version (shortened)
    let version = if state.rpc_data.client_version.is_empty() {
        "...".to_string()
    } else {
        state.rpc_data.client_version.replace("Monad/", "v")
    };

    // Error or status
    let status = if let Some(ref err) = state.last_error {
        Span::styled(format!("⚠ {}", err), Style::default().fg(Color::Red))
    } else {
        let time_since = state
            .time_since_last_block()
            .map(|d| format!("{:.1}s", d.as_secs_f64()))
            .unwrap_or_else(|| "...".to_string());
        Span::styled(format!("last: {}", time_since), Style::default().fg(LABEL_COLOR))
    };

    let footer = Line::from(vec![
        Span::styled("UPTIME: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(uptime, Style::default().fg(VALUE_COLOR)),
        Span::raw("  |  "),
        Span::styled("GAS: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(format!("{:.0}gwei", gas_gwei), Style::default().fg(VALUE_COLOR)),
        Span::raw("  |  "),
        Span::styled(version, Style::default().fg(LABEL_COLOR)),
        Span::raw("  |  "),
        status,
        Span::raw("  |  "),
        Span::styled("q: quit", Style::default().fg(LABEL_COLOR)),
    ]);

    frame.render_widget(Paragraph::new(footer), inner);
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.insert(0, ',');
        }
        result.insert(0, c);
    }
    result
}
