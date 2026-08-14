# Contributing to monad-monitor

## How work lands

All changes go through a pull request, and every pull request needs an approving
review from @portdeveloper before it can merge. Direct pushes to the default
branch are turned off. A merge means the work was read and accepted, not just
that it was opened.

Thank you for your interest in contributing to monad-monitor - a lightweight terminal UI for real-time monitoring of Monad blockchain nodes.

## About the project

monad-monitor sits in a terminal next to a running Monad node and shows what the node is doing: block height, TPS, peers, latency, system stats, and recent blocks.

**Stack:** Rust (stable) + ratatui/crossterm for the TUI, tokio for async, reqwest for the Prometheus scrape, tokio-tungstenite for the WebSocket subscription.

### Read-only by design

The monitor observes a node; it never sends transactions, never signs anything, and never mutates node state. Every contribution must keep that property. New data sources should be reads (metrics endpoints, RPC queries, log files), never writes.

### Project status

Published on [crates.io](https://crates.io/crates/monad-monitor) and under active development. Good areas to contribute:

- New metrics and panels (consensus, mempool, execution)
- Exporters (Prometheus re-export, JSON output, headless mode)
- Alerting (thresholds, desktop/webhook notifications)
- RPC probes (health checks against the node's endpoints)
- Themes and rendering improvements

## Getting started

```bash
git clone https://github.com/portdeveloper/monad-monitor
cd monad-monitor
cargo build
cargo run
```

To see real data you need a Monad node exposing:

- Prometheus metrics on `http://localhost:8889/metrics`
- A WebSocket endpoint on `ws://localhost:8081` (see the [Monad events and WebSockets docs](https://docs.monad.xyz/node-ops/events-and-websockets))

## How to contribute

Contributions are welcome via Issues and Pull Requests.

- **Report bugs** or **suggest features** by opening an Issue.
- **Add a metric or panel** that node operators actually watch.
- **Improve robustness**: reconnects, error states, partial data.
- **Fix bugs** or improve rendering on unusual terminal sizes.

### Guidelines

- Search for existing Issues and PRs before creating your own.
- Each contribution should focus on one thing - don't mix a feature with style fixes.
- Keep dependencies minimal; this is a tool people run on production node hosts.
- Degrade gracefully: if a data source is missing, show a clear empty state instead of crashing.

### Code layout

The codebase is small and flat; read the file you're touching end to end first:

- `src/main.rs` - entry point, event loop, key handling.
- `src/metrics.rs` - Prometheus scrape and parsing (`PrometheusMetrics`).
- `src/rpc.rs` - WebSocket subscriptions for real-time blocks.
- `src/system.rs` - host stats (CPU, memory, disk, network, services).
- `src/state.rs` - application state, history buffers, trend tracking.
- `src/ui.rs` - all ratatui rendering, layouts, and themes.

New data flows follow the same shape: fetch in its own module, merge into `state.rs`, render in `ui.rs`.

### Rules

1. **Every PR requires an approved Issue first.** Open an Issue describing what you want to do and wait for a maintainer to agree before writing code. PRs without a linked, approved Issue will be closed.
2. Contributors must be humans, not bots.
3. First-time contributions must not be only spelling, grammar, or formatting fixes.
4. If you contribute with an AI agent, the agent must read and follow <https://most.devnads.com/agents.md>: wait for approval before writing code, one PR per day.
5. **One claimed issue per person at a time, across all pool repos.** A claim reserves the issue once a maintainer approves it. Claiming a second issue while you already hold one voids all of your claims, and a claim with no PR or progress update for 7 days gets released so someone else can take it.

### Issues

Open an Issue before doing any work. Describe what you want to change and why. This lets us discuss the approach, avoid duplicate effort, and say no early if the change doesn't fit.

When reporting a bug:

- Describe what you expected vs what happened.
- Include your terminal, OS, and node setup if relevant.
- A screenshot or a paste of the broken frame helps.

### Pull requests

Once your Issue is approved, follow the fork-and-pull workflow:

1. Fork the repo
2. Create a branch with a descriptive name
3. Make your changes
4. Run `cargo fmt`, `cargo clippy`, and `cargo build --release` before pushing
5. Push to your fork and open a PR

Tips for a good PR:

- Keep the title short and descriptive.
- Link the approved Issue.
- Describe what changed and why; screenshots of the TUI are welcome.
- One commit per logical change is fine; we squash-merge.

PRs without a linked Issue, or that change things not discussed in the Issue, will be closed. After review, we may ask questions or request changes. Once approved, we'll squash-and-merge.
