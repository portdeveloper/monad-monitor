# monad-monitor

A lightweight terminal UI (TUI) for real-time monitoring of Monad blockchain nodes.

![Rust](https://img.shields.io/badge/rust-stable-orange)
[![Crates.io](https://img.shields.io/crates/v/monad-monitor)](https://crates.io/crates/monad-monitor)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

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
