use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Sparkline, Table},
    Frame,
};

use crate::state::AppState;

const MONAD_PURPLE: Color = Color::Rgb(100, 60, 180);
const MONAD_ACCENT: Color = Color::Rgb(120, 80, 200);
const TITLE_COLOR: Color = Color::Black;               // Titles
const LABEL_COLOR: Color = Color::Rgb(80, 80, 80);     // Labels and borders
const VALUE_COLOR: Color = Color::Rgb(40, 40, 40);     // Values - near black
const TEXT_DIM: Color = Color::Rgb(60, 60, 60);        // Dimmed data text

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
    let block = Block::default()
        .title(" monad-monitor ")
        .title_style(Style::default().fg(TITLE_COLOR).bold())
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

    // Peers
    let peer_count = state.metrics.peer_count;
    let peer_health = state.peer_health();
    let peer_color = match peer_health {
        "healthy" => Color::Green,
        "ok" => Color::Yellow,
        _ => Color::Red,
    };

    let peer_text = vec![
        Line::from(Span::styled("PEERS", Style::default().fg(LABEL_COLOR))),
        Line::from(Span::styled(
            format!("{}", peer_count),
            Style::default().fg(VALUE_COLOR).bold(),
        )),
        Line::from(vec![
            Span::styled("↑ ", Style::default().fg(peer_color)),
            Span::styled(peer_health, Style::default().fg(peer_color)),
        ]),
    ];
    frame.render_widget(Paragraph::new(peer_text).alignment(Alignment::Center), columns[1]);

    // TPS
    let tps = state.tps;
    let tps_text = vec![
        Line::from(Span::styled("TPS", Style::default().fg(LABEL_COLOR))),
        Line::from(Span::styled(
            format!("{:.0}", tps),
            Style::default().fg(MONAD_ACCENT).bold(),
        )),
        Line::from(Span::styled("tx/sec", Style::default().fg(LABEL_COLOR))),
    ];
    frame.render_widget(Paragraph::new(tps_text).alignment(Alignment::Center), columns[2]);

    // Latency (p99)
    let latency = state.metrics.latency_p99_ms;
    let latency_color = if latency < 100 {
        Color::Green
    } else if latency < 500 {
        Color::Yellow
    } else {
        Color::Red
    };

    let latency_text = vec![
        Line::from(Span::styled("LATENCY", Style::default().fg(LABEL_COLOR))),
        Line::from(Span::styled(
            format!("{}ms", latency),
            Style::default().fg(latency_color).bold(),
        )),
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
        Span::raw("  │  "),
        Span::styled("SERVICES: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(services_str, Style::default().fg(services_color)),
        Span::raw("  │  "),
        Span::styled("FINALIZED: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(format!("-{}", fin_lag), Style::default().fg(lag_color)),
        Span::styled(format!(" (ver -{})", ver_lag), Style::default().fg(LABEL_COLOR)),
        Span::raw("  │  "),
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
        .style(Style::default().fg(MONAD_PURPLE))
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

    let blocks = state.recent_blocks();
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let rows: Vec<Row> = blocks
        .iter()
        .map(|b| {
            let hash_short = if b.hash.len() > 14 {
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

            Row::new(vec![
                format!("#{}", format_number(b.number)),
                format!("{} txs", b.tx_count),
                hash_short,
                format!("{:.0}% gas", gas_pct),
                age,
            ])
            .style(Style::default().fg(TEXT_DIM))
        })
        .collect();

    let widths = [
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(16),
        Constraint::Length(10),
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

    // Pending TXs
    let pending = state.metrics.pending_txs;

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
        Span::raw("  │  "),
        Span::styled("PENDING: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(format!("{} tx", pending), Style::default().fg(VALUE_COLOR)),
        Span::raw("  │  "),
        Span::styled("GAS: ", Style::default().fg(LABEL_COLOR)),
        Span::styled(format!("{:.0}gwei", gas_gwei), Style::default().fg(VALUE_COLOR)),
        Span::raw("  │  "),
        Span::styled(version, Style::default().fg(LABEL_COLOR)),
        Span::raw("  │  "),
        status,
        Span::raw("  │  "),
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
