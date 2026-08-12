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

Or point it at a node on another host or on other ports:

```bash
monad-monitor --metrics-url http://node:8889/metrics --ws-url ws://node:8081
```

### Endpoint options

Every default is the value the monitor used to hardcode, so a run with no flags
behaves exactly as it did before. `monad-monitor --help` lists every flag,
including the alert thresholds further down.

| Flag | Default | What it sets |
|------|---------|--------------|
| `--metrics-url <URL>` | `http://localhost:8889/metrics` | Prometheus endpoint the monitor scrapes |
| `--ws-url <URL>` | `ws://localhost:8081` | The node's WebSocket, for real-time blocks |
| `--refresh <secs>` | `1` | How often the metrics endpoint is scraped (TUI only) |
| `--network <name>` | `mainnet` | Network whose public node the block height is compared against, as `wss://rpc-<name>.monadinfra.com` |
| `--external-rpc-url <URL>` | built from `--network` | That comparison endpoint, given outright |

A URL is checked where it is given, so `--ws-url http://node:8081` is refused up
front rather than failing later as a WebSocket that never connects. An
unreachable endpoint reports the URL it tried.

### Config file

The same settings can live in a file, so a host with a fixed setup does not need
the flags on every run. The monitor reads
`$XDG_CONFIG_HOME/monad-monitor/config.toml`, falling back to
`~/.config/monad-monitor/config.toml` when `XDG_CONFIG_HOME` is unset. The file
is optional.

```toml
metrics_url = "http://node:8889/metrics"
ws_url = "ws://node:8081"
refresh = 5
network = "testnet"
# external_rpc_url = "wss://rpc-testnet.monadinfra.com"
```

Keys are the flag names with underscores. Precedence runs one way: a flag beats
the file and the file beats the default. An unset key keeps its default. An
unknown key is an error naming the line, so a typo cannot leave you watching
localhost while you believe you pointed the monitor somewhere else.

### Requirements

Your Monad node must expose:
- **Prometheus metrics** on `http://localhost:8889/metrics`
- **WebSocket endpoint** on `ws://localhost:8081` (used for real-time block subscriptions)

> **Note:** WebSocket support must be enabled on your node. See the [Monad Events and WebSockets documentation](https://docs.monad.xyz/node-ops/events-and-websockets) for setup instructions.

### Keyboard Controls

| Key | Action |
|-----|--------|
| `q` / `Q` / `Esc` | Quit |
| `t` / `T` | Cycle through themes |

## Alerts

Every threshold is off by default, so running without these flags behaves exactly as before. Set the ones you care about and the monitor notifies a webhook when a threshold trips, and again when it clears.

| Flag | Effect |
|------|--------|
| `--webhook-url URL` | Where to POST. Without it, a tripped threshold only colours the TUI |
| `--alert-no-block SECS` | No new block over the node's WebSocket for this many seconds |
| `--alert-finalized-lag N` | Finalized lag grows past N blocks |
| `--alert-min-peers N` | Peer count drops below N |
| `--alert-disk PCT` | Disk usage rises above this percentage |
| `--alert-confirm N` | Seconds a change must hold before it counts (default 3) |
| `--alert-cooldown N` | Shortest gap between two alerts for the same threshold (default 300) |

```bash
monad-monitor \
  --webhook-url https://discord.com/api/webhooks/... \
  --alert-no-block 30 \
  --alert-min-peers 10 \
  --alert-disk 85
```

An incident is two messages, one when the threshold trips and one when it recovers, never a message per refresh. While an alert is up, the affected cell turns red on screen.

Two settings keep a noisy metric from filling a channel. `--alert-confirm` absorbs brief crossings: a change has to hold that many seconds before it counts. `--alert-cooldown` absorbs the harder case, a value that genuinely drifts back and forth across its threshold for minutes at a time; once a threshold has alerted it stays quiet for the cooldown, so a drifting metric costs one pair per cooldown rather than one per crossing. A recovery is never held back, so an incident that was announced is always closed.

`--alert-no-block` watches the WebSocket block stream specifically, so a node that stops delivering blocks trips it even while the metrics endpoint keeps answering.

The payload carries the same human-readable line in `content` and in `text`, which is what Discord and Slack render respectively, so one webhook URL works for either without a translation layer. The structured fields are there for anything else consuming the hook:

```json
{
  "content": "🔴 MFNode: peer count (3 peers, threshold 10 peers)",
  "text": "🔴 MFNode: peer count (3 peers, threshold 10 peers)",
  "alert": "low_peers",
  "status": "firing",
  "value": "3 peers",
  "threshold": "10 peers",
  "node": "MFNode"
}
```

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
