# monad-monitor

A lightweight terminal UI (TUI) for real-time monitoring of Monad blockchain nodes.

![Rust](https://img.shields.io/badge/rust-stable-orange)
[![Crates.io](https://img.shields.io/crates/v/monad-monitor)](https://crates.io/crates/monad-monitor)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> ⚠️ **Disclaimer:** This software is provided "as-is" without warranty of any kind. It has not been audited for security vulnerabilities. Use at your own risk. The authors assume no liability for any damages arising from the use of this software.

## Features

- **Real-time metrics** - Block height, TPS, peer count, network latency
- **System monitoring** - CPU, memory, disk usage, network bandwidth
- **TPS sparkline** - Visual history of transactions per second
- **Recent blocks table** - Latest blocks with gas usage visualization
- **5 color themes** - Gray, Light, Monad (purple), Matrix (green), Ocean (blue)
- **Heartbeat animation** - Pulsing indicator based on block arrival

## Installation

### From crates.io

```bash
cargo install monad-monitor
```

### From source

```bash
git clone https://github.com/portdeveloper/monad-monitor
cd monad-monitor
cargo build --release
```

## Usage

Run on a machine with a Monad node:

```bash
monad-monitor
```

### Alerts

All thresholds are off by default. Enable any of them with flags:

```bash
monad-monitor \
  --alert-block-stall 30 \
  --alert-finalized-lag 10 \
  --alert-min-peers 5 \
  --alert-disk-pct 90 \
  --webhook-url https://discord.com/api/webhooks/...
```

| Flag | Meaning |
|------|---------|
| `--alert-block-stall <secs>` | Alert when no new block for this many seconds |
| `--alert-finalized-lag <blocks>` | Alert when finalized lag exceeds this many blocks |
| `--alert-min-peers <n>` | Alert when peer count drops below this |
| `--alert-disk-pct <pct>` | Alert when disk usage exceeds this percentage |
| `--webhook-url <url>` | POST a JSON payload here on every alert transition |

The same settings can live in `~/.config/monad-monitor/config.toml`
(or `$XDG_CONFIG_HOME/monad-monitor/config.toml`); flags take precedence:

```toml
alert_block_stall = 30
alert_finalized_lag = 10
alert_min_peers = 5
alert_disk_pct = 90
webhook_url = "https://discord.com/api/webhooks/..."
```

Alerts fire on state transitions only: one webhook POST when a threshold
trips and one when it recovers, not one per refresh tick. While an alert is
firing the affected cell in the TUI turns red. The payload is generic JSON
with a human-readable `text` field (duplicated as `content`), so pointing
`--webhook-url` directly at a Discord or Slack incoming webhook renders a
readable message:

```json
{
  "alert": "block_stall",
  "state": "firing",
  "value": 45.2,
  "threshold": 30,
  "node": "my-node",
  "timestamp": 1753670000,
  "text": "[monad-monitor] block_stall firing on my-node: no new block for 45.2s (threshold 30s)",
  "content": "[monad-monitor] block_stall firing on my-node: no new block for 45.2s (threshold 30s)"
}
```

Alerting stays read-only: it observes and notifies, nothing more. A failed
webhook delivery shows up in the footer and never interrupts monitoring.

### Requirements

Your Monad node must expose:
- **Prometheus metrics** on `http://localhost:8889/metrics`
- **WebSocket endpoint** on `ws://localhost:8080` (used for real-time block subscriptions)

> **Note:** WebSocket support must be enabled on your node. See the [Monad Events and WebSockets documentation](https://docs.monad.xyz/node-ops/events-and-websockets) for setup instructions.

### Keyboard Controls

| Key | Action |
|-----|--------|
| `q` / `Q` / `Esc` | Quit |
| `t` / `T` | Cycle through themes |

## Display

```
┌─────────────────────────────────────────────────────────┐
│  BLOCK        PEERS       TPS          LATENCY         │
│  12,345,678   45 ▲        1,234 ▲      12ms ▼          │
├─────────────────────────────────────────────────────────┤
│  CPU 23%  MEM 45%  DISK 67%  NET ↑12MB/s ↓8MB/s        │
├─────────────────────────────────────────────────────────┤
│  TPS ████▄▂▁▃▆████▇▅▃▂▁▂▄▆███                          │
├─────────────────────────────────────────────────────────┤
│  Block      Hash          Txs    Gas Used              │
│  12345678   0xabc...def   150    ████████░░ 82%        │
│  12345677   0x123...456   142    ███████░░░ 75%        │
└─────────────────────────────────────────────────────────┘
```

## Metrics Displayed

### Header
- **Block height** - Current block number with sync status
- **Peers** - Connected peer count with trend indicator
- **TPS** - Transactions per second with peak tracking
- **Latency** - Network latency (p99) with trend indicator

### System Stats
- CPU / Memory / Disk usage
- Network bandwidth (upload/download)
- Service status (monad-node, monad-mpt)
- Finalized block lag

### Block Table
- Block number and hash
- Transaction count
- Gas used with visual bar

## License

MIT License - see [LICENSE](LICENSE) for details.
