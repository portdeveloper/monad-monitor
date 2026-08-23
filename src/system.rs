use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::json;
use std::fs;
use std::process::Command;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// How long the comparison against the public node may take, connect and read
/// together. It sits inside the system refresh, so a stalled endpoint costs one
/// unknown reading rather than every system stat behind it.
const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Data from system commands (monad-mpt, systemctl, external RPC)
#[derive(Debug, Clone, Default, Serialize)]
pub struct SystemData {
    // Disk info from monad-mpt
    pub disk_capacity_gb: f64,
    pub disk_used_gb: f64,
    pub disk_used_pct: f64,
    pub history_count: u64,
    pub history_earliest: u64,
    pub history_latest: u64,
    pub latest_finalized: u64,
    pub latest_verified: u64,

    // Services status
    pub service_bft: bool,
    pub service_execution: bool,
    pub service_rpc: bool,

    /// The public node's height, for the comparison in the header. `None` when
    /// that endpoint could not be reached in time: unknown is a state of its
    /// own, and reporting it as zero reads on screen as "no difference".
    pub external_block: Option<u64>,

    // System resources
    pub memory_used_pct: f64,
    pub memory_used_gb: f64,
    pub memory_total_gb: f64,
    /// Busy percentage over the interval between the last two samples. `None`
    /// until a second sample exists, because a rate needs two readings and one
    /// reading can only produce an average over all of uptime.
    pub cpu_usage_pct: Option<f64>,

    // Network (bytes since boot, for calculating rate)
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,

    // Node identifier (hostname)
    pub node_id: String,

    // Service start time (seconds since epoch)
    pub service_started_at: u64,
}

impl SystemData {
    pub fn block_difference(&self, local_block: u64) -> Option<i64> {
        Some(self.external_block? as i64 - local_block as i64)
    }

    pub fn finalized_lag(&self) -> u64 {
        self.history_latest.saturating_sub(self.latest_finalized)
    }

    pub fn all_services_running(&self) -> bool {
        self.service_bft && self.service_execution && self.service_rpc
    }

    /// Returns formatted uptime since service restart
    pub fn uptime_since_restart(&self) -> String {
        if self.service_started_at == 0 {
            return "...".to_string();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if now < self.service_started_at {
            return "...".to_string();
        }

        let elapsed = now - self.service_started_at;
        let days = elapsed / 86400;
        let hours = (elapsed % 86400) / 3600;
        let mins = (elapsed % 3600) / 60;

        if days > 0 {
            format!("{}d {}h", days, hours)
        } else if hours > 0 {
            format!("{}h {}m", hours, mins)
        } else {
            format!("{}m", mins)
        }
    }
}

/// The cumulative CPU counters from /proc/stat. They only mean something as a
/// difference between two readings, so they are kept rather than turned into a
/// percentage on the spot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CpuTimes {
    /// Time not spent working: idle plus iowait.
    idle: u64,
    /// All accounted time.
    total: u64,
}

pub struct SystemClient {
    /// The public node the local block height is compared against. The config
    /// layer resolves it, so this never has to know about network names.
    external_rpc_url: String,
    /// The previous CPU reading, held so each sample can be compared with the
    /// one before it.
    cpu_prev: Option<CpuTimes>,
}

impl SystemClient {
    pub fn new(external_rpc_url: &str) -> Self {
        Self {
            external_rpc_url: external_rpc_url.to_string(),
            cpu_prev: None,
        }
    }

    pub async fn fetch(&mut self) -> Result<SystemData> {
        let mut data = SystemData::default();

        // Fetch monad-mpt data (blocking, but fast)
        if let Ok(mpt_output) = tokio::task::spawn_blocking(|| {
            Command::new("monad-mpt")
                .args(["--storage", "/dev/triedb"])
                .output()
        })
        .await?
        {
            let output = String::from_utf8_lossy(&mpt_output.stdout);
            parse_mpt_output(&output, &mut data);
        }

        // Fetch services status (blocking, but fast)
        if let Ok(services) = tokio::task::spawn_blocking(fetch_services_status).await {
            data.service_bft = services.0;
            data.service_execution = services.1;
            data.service_rpc = services.2;
            data.service_started_at = services.3;
        }

        // Fetch external block number. This is the one reading that depends on
        // a host neither the operator nor this monitor controls, so it is kept
        // on a leash: an endpoint that accepts the connection and then says
        // nothing must not take the rest of the fetch down with it.
        data.external_block = timeout(EXTERNAL_TIMEOUT, self.fetch_external_block())
            .await
            .ok()
            .and_then(|result| result.ok());

        // Fetch system resources (blocking, but fast)
        if let Ok(resources) = tokio::task::spawn_blocking(fetch_system_resources).await {
            data.memory_used_pct = resources.0;
            data.memory_used_gb = resources.1;
            data.memory_total_gb = resources.2;
            data.net_rx_bytes = resources.4;
            data.net_tx_bytes = resources.5;

            // CPU is a rate, so it comes from the change since the previous
            // sample. The first sample has nothing to compare against and
            // reports nothing rather than an average over all of uptime.
            let now = resources.3;
            data.cpu_usage_pct = match (self.cpu_prev, now) {
                (Some(prev), Some(now)) => cpu_percentage(prev, now),
                _ => None,
            };
            if now.is_some() {
                self.cpu_prev = now;
            }
        }

        // Fetch hostname
        if let Ok(hostname) = fs::read_to_string("/etc/hostname") {
            data.node_id = hostname.trim().to_string();
        }

        Ok(data)
    }

    async fn fetch_external_block(&self) -> Result<u64> {
        let (ws_stream, _) = connect_async(&self.external_rpc_url)
            .await
            .with_context(|| {
                format!(
                    "Failed to connect to external WebSocket {}",
                    self.external_rpc_url
                )
            })?;

        let (mut write, mut read) = ws_stream.split();

        let request = json!({
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": 1
        });

        write
            .send(Message::Text(request.to_string()))
            .await
            .context("Failed to send WebSocket message")?;

        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let response: serde_json::Value = serde_json::from_str(&text)?;
                    let hex = response["result"]
                        .as_str()
                        .context("External node did not answer eth_blockNumber with a height")?;
                    return parse_hex_block(hex);
                }
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => continue,
            }
        }
        anyhow::bail!("External node closed the connection before answering")
    }
}

/// Reads the hex block number out of an `eth_blockNumber` reply. A value that
/// does not parse is refused rather than turned into zero, so a malformed reply
/// leaves the comparison unknown instead of claiming the public node is at
/// genesis.
fn parse_hex_block(hex: &str) -> Result<u64> {
    let digits = hex.strip_prefix("0x").unwrap_or(hex);
    u64::from_str_radix(digits, 16)
        .with_context(|| format!("Unreadable block number from external node: {}", hex))
}

/// Returns (bft_active, execution_active, rpc_active, started_at_timestamp)
fn fetch_services_status() -> (bool, bool, bool, u64) {
    let bft = Command::new("systemctl")
        .args(["is-active", "--quiet", "monad-bft"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let execution = Command::new("systemctl")
        .args(["is-active", "--quiet", "monad-execution"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let rpc = Command::new("systemctl")
        .args(["is-active", "--quiet", "monad-rpc"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    (bft, execution, rpc, service_started_at())
}

/// The epoch second monad-bft last became active.
///
/// systemd knows this exactly, so it is asked for it rather than having its
/// localized output reconstructed: a printed timestamp carries a time zone that
/// has to be interpreted, and interpreting it wrongly is silent.
fn service_started_at() -> u64 {
    let show = |args: &[&str]| {
        Command::new("systemctl")
            .args(args)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
    };

    // systemd 251 and newer hand back the epoch directly.
    if let Some(started) = show(&[
        "show",
        "monad-bft",
        "--property=ActiveEnterTimestamp",
        "--timestamp=unix",
    ])
    .and_then(|out| parse_unix_timestamp(&out))
    {
        return started;
    }

    // Older systemd rejects that flag (Ubuntu 22.04 ships 249), so derive the
    // epoch from the monotonic clock and the boot time, which every supported
    // version reports.
    match (
        show(&[
            "show",
            "monad-bft",
            "--property=ActiveEnterTimestampMonotonic",
        ]),
        boot_time(),
    ) {
        (Some(out), Some(boot)) => parse_monotonic_timestamp(&out, boot).unwrap_or(0),
        // Unknown beats guessed: 0 renders as "..." rather than a wrong number.
        _ => 0,
    }
}

/// The value side of a `systemctl show --property=X` line, or `None` when the
/// property is missing, empty, or one of systemd's "no such moment" markers. A
/// unit that does not exist exits 0 with an empty value, so the exit status
/// alone would not catch it.
fn property_value(output: &str) -> Option<&str> {
    let value = output.split_once('=')?.1.trim();
    if value.is_empty() || value == "n/a" || value == "never" {
        return None;
    }
    Some(value)
}

/// Parses `ActiveEnterTimestamp=@1785564987`, the form systemd prints under
/// `--timestamp=unix`. An epoch needs no calendar and no time zone.
fn parse_unix_timestamp(output: &str) -> Option<u64> {
    // Only the @epoch form is accepted. A systemd too old for the flag ignores
    // it and prints its localized string instead, and reading that as a moment
    // in time is exactly the guesswork this replaces.
    property_value(output)?.strip_prefix('@')?.parse().ok()
}

/// Parses `ActiveEnterTimestampMonotonic=…`, microseconds since boot, into an
/// epoch using the boot time.
fn parse_monotonic_timestamp(output: &str, boot_time: u64) -> Option<u64> {
    let micros: u64 = property_value(output)?.parse().ok()?;
    // Zero means the unit has never been activated. Added to the boot time it
    // would claim the service came up the instant the machine did.
    if micros == 0 {
        return None;
    }
    Some(boot_time + micros / 1_000_000)
}

/// Boot time as an epoch, from `btime` in /proc/stat.
fn boot_time() -> Option<u64> {
    parse_btime(&fs::read_to_string("/proc/stat").ok()?)
}

fn parse_btime(stat: &str) -> Option<u64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse().ok())
}

/// Returns (mem_pct, mem_used_gb, mem_total_gb, cpu_times, net_rx, net_tx)
fn fetch_system_resources() -> (f64, f64, f64, Option<CpuTimes>, u64, u64) {
    let mut mem_pct = 0.0;
    let mut mem_used_gb = 0.0;
    let mut mem_total_gb = 0.0;
    let mut net_rx: u64 = 0;
    let mut net_tx: u64 = 0;

    // Parse /proc/meminfo for memory
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        let mut total_kb: u64 = 0;
        let mut available_kb: u64 = 0;

        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                total_kb = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if line.starts_with("MemAvailable:") {
                available_kb = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            }
        }

        if total_kb > 0 {
            let used_kb = total_kb.saturating_sub(available_kb);
            mem_total_gb = total_kb as f64 / 1024.0 / 1024.0;
            mem_used_gb = used_kb as f64 / 1024.0 / 1024.0;
            mem_pct = (used_kb as f64 / total_kb as f64) * 100.0;
        }
    }

    // Read the CPU counters. They are cumulative, so the percentage is worked
    // out one level up, from the difference against the previous reading.
    let cpu_times = fs::read_to_string("/proc/stat")
        .ok()
        .as_deref()
        .and_then(parse_cpu_times);

    // Parse /proc/net/dev for network stats (sum all interfaces except lo)
    if let Ok(netdev) = fs::read_to_string("/proc/net/dev") {
        for line in netdev.lines().skip(2) {
            let line = line.trim();
            if line.starts_with("lo:") {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 10 {
                // Format: iface: rx_bytes rx_packets ... tx_bytes tx_packets ...
                if let Ok(rx) = parts[1].parse::<u64>() {
                    net_rx += rx;
                }
                if let Ok(tx) = parts[9].parse::<u64>() {
                    net_tx += tx;
                }
            }
        }
    }

    (
        mem_pct,
        mem_used_gb,
        mem_total_gb,
        cpu_times,
        net_rx,
        net_tx,
    )
}

/// Reads the aggregate `cpu` line of /proc/stat.
///
/// The fields are cumulative jiffies since boot: user, nice, system, idle,
/// iowait, irq, softirq, steal, and then guest and guest_nice. The guest pair is
/// already included in user and nice, so summing every field would count that
/// time twice; only the first eight are added up.
fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let fields: Vec<u64> = stat
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .filter_map(|field| field.parse().ok())
        .collect();

    if fields.len() < 5 {
        return None;
    }

    Some(CpuTimes {
        // iowait is the CPU waiting on storage, not working, so it belongs with
        // idle. Counting it as busy is what made a node doing heavy disk I/O
        // look pegged.
        idle: fields[3] + fields[4],
        total: fields.iter().take(8).sum(),
    })
}

/// Busy percentage over the interval between two readings.
fn cpu_percentage(prev: CpuTimes, now: CpuTimes) -> Option<f64> {
    // Counters only move forward; anything else means the reading is not
    // comparable, so report nothing rather than a number from a bad subtraction.
    let total = now.total.checked_sub(prev.total)?;
    let idle = now.idle.checked_sub(prev.idle)?;
    if total == 0 || idle > total {
        return None;
    }

    Some((total - idle) as f64 / total as f64 * 100.0)
}

fn is_size_unit(s: &str) -> bool {
    matches!(s, "Tb" | "Gb" | "Mb" | "Kb")
}

fn size_to_gb(value: f64, unit: &str) -> f64 {
    match unit {
        "Tb" => value * 1024.0,
        "Mb" => value / 1024.0,
        "Kb" => value / (1024.0 * 1024.0),
        _ => value, // Gb, or an unrecognised unit left as-is
    }
}

fn trim_num(s: &str) -> &str {
    s.trim_end_matches([',', '.'])
}

fn parse_mpt_output(output: &str, data: &mut SystemData) {
    for line in output.lines() {
        let line = line.trim();

        // Parse disk capacity/used line. Capacity and used each carry their own
        // unit, and the used unit grows with size:
        //   "1.75 Tb   109.30 Gb   6.11%   /dev/..."   (used below 1 Tb)
        //   "1.82 Tb     1.10 Tb  60.21%   /dev/..."   (used at or above 1 Tb)
        // Match on the column shape (num unit num unit pct%) so a used value in
        // Tb parses too, rather than requiring a "Gb" somewhere on the line.
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5
            && is_size_unit(parts[1])
            && is_size_unit(parts[3])
            && parts[4].ends_with('%')
        {
            if let Ok(cap) = parts[0].parse::<f64>() {
                data.disk_capacity_gb = size_to_gb(cap, parts[1]);
            }
            if let Ok(used) = parts[2].parse::<f64>() {
                data.disk_used_gb = size_to_gb(used, parts[3]);
            }
            if let Ok(pct) = parts[4].trim_end_matches('%').parse::<f64>() {
                data.disk_used_pct = pct;
            }
        }

        // Parse history line. Older clients print it on the summary line, newer
        // ones print it under the timeline:
        //   old: "MPT database has 637751 history, earliest is 41295350 latest is 41933100."
        //   new: "History: 7973666 versions, earliest is 41193452, latest is 49167117"
        // Both spell out "earliest is X" and "latest is Y"; the count follows
        // "has" or "History:". Numbers may carry a trailing comma or period.
        let is_history_line = (line.contains("MPT database has") && line.contains("history"))
            || line.starts_with("History:");
        if is_history_line {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for (i, part) in parts.iter().enumerate() {
                if (*part == "has" || *part == "History:") && i + 1 < parts.len() {
                    data.history_count = trim_num(parts[i + 1]).parse().unwrap_or(0);
                }
                if *part == "earliest" && i + 2 < parts.len() {
                    data.history_earliest = trim_num(parts[i + 2]).parse().unwrap_or(0);
                }
                if *part == "latest" && i + 2 < parts.len() {
                    data.history_latest = trim_num(parts[i + 2]).parse().unwrap_or(0);
                }
            }
        }

        // Parse finalized/verified: "Latest finalized is 41933098, latest verified is 41933095"
        if line.contains("Latest finalized is") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for (i, part) in parts.iter().enumerate() {
                if *part == "finalized" && i + 2 < parts.len() {
                    let val = parts[i + 2].trim_end_matches(',');
                    data.latest_finalized = val.parse().unwrap_or(0);
                }
                if *part == "verified" && i + 2 < parts.len() {
                    let val = parts[i + 2].trim_end_matches(',');
                    data.latest_verified = val.parse().unwrap_or(0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_line_used_in_terabytes() {
        // Once used passes 1 Tb the used column is also Tb; the old guard needed
        // a "Gb" on the line, missed this, and left the disk read at 0%.
        let out = "           1.75 Tb        1.31 Tb 75.24%  \"/dev/nvme2n1p1\"";
        let mut data = SystemData::default();
        parse_mpt_output(out, &mut data);
        assert_eq!(data.disk_used_pct, 75.24);
        assert!((data.disk_capacity_gb - 1.75 * 1024.0).abs() < 1e-6);
        assert!((data.disk_used_gb - 1.31 * 1024.0).abs() < 1e-6);
    }

    #[test]
    fn disk_line_used_in_gigabytes() {
        let out = "           1.75 Tb      109.30 Gb  6.11%  \"/dev/nvme2n1p1\"";
        let mut data = SystemData::default();
        parse_mpt_output(out, &mut data);
        assert_eq!(data.disk_used_pct, 6.11);
        assert!((data.disk_capacity_gb - 1.75 * 1024.0).abs() < 1e-6);
        assert!((data.disk_used_gb - 109.30).abs() < 1e-6);
    }

    #[test]
    fn history_versions_line() {
        // Current clients print history under the timeline as "History: ...".
        let out = "        History: 7973666 versions, earliest is 41193452, latest is 49167117";
        let mut data = SystemData::default();
        parse_mpt_output(out, &mut data);
        assert_eq!(data.history_count, 7973666);
        assert_eq!(data.history_earliest, 41193452);
        assert_eq!(data.history_latest, 49167117);
    }

    #[test]
    fn history_legacy_line() {
        let out = "MPT database has 637751 history, earliest is 41295350 latest is 41933100.";
        let mut data = SystemData::default();
        parse_mpt_output(out, &mut data);
        assert_eq!(data.history_count, 637751);
        assert_eq!(data.history_earliest, 41295350);
        assert_eq!(data.history_latest, 41933100);
    }

    #[test]
    fn finalized_lag_recovers_with_history_line() {
        // With the "History:" line parsed, finalized_lag stops reading a flat 0.
        let out = "        History: 100 versions, earliest is 1, latest is 500\n     \
                   Latest finalized is 498, latest verified is 495";
        let mut data = SystemData::default();
        parse_mpt_output(out, &mut data);
        assert_eq!(data.history_latest, 500);
        assert_eq!(data.latest_finalized, 498);
        assert_eq!(data.finalized_lag(), 2);
    }

    #[test]
    fn the_unix_form_is_taken_as_an_epoch() {
        // systemd 251 and newer, with --timestamp=unix. Straight from a
        // mainnet node.
        let out = "ActiveEnterTimestamp=@1785564987\n";
        assert_eq!(parse_unix_timestamp(out), Some(1785564987));
    }

    #[test]
    fn the_localized_string_is_refused_rather_than_reconstructed() {
        // What systemd prints without the flag, and what the previous
        // implementation rebuilt an epoch from. Turning this into a moment in
        // time means assuming a time zone, which is the bug being replaced, so
        // it has to be rejected rather than parsed loosely.
        let out = "ActiveEnterTimestamp=Sat 2026-08-01 06:16:27 UTC\n";
        assert_eq!(parse_unix_timestamp(out), None);
    }

    #[test]
    fn an_absent_or_unset_property_is_unknown() {
        // A unit that does not exist: systemctl exits 0 with an empty value, so
        // the exit status alone would not catch it.
        assert_eq!(parse_unix_timestamp("ActiveEnterTimestamp=\n"), None);
        assert_eq!(parse_unix_timestamp("ActiveEnterTimestamp=n/a\n"), None);
        assert_eq!(parse_unix_timestamp("ActiveEnterTimestamp=@\n"), None);
        assert_eq!(parse_unix_timestamp("no equals sign here"), None);
    }

    #[test]
    fn the_monotonic_form_is_offset_from_boot() {
        // Microseconds since boot: 2h 30m after a machine that booted at
        // 1785000000.
        let out = "ActiveEnterTimestampMonotonic=9000000000\n";
        assert_eq!(
            parse_monotonic_timestamp(out, 1_785_000_000),
            Some(1_785_009_000)
        );
    }

    #[test]
    fn a_never_activated_unit_does_not_inherit_the_boot_time() {
        // Monotonic is 0 for a unit that has never come up. Added to the boot
        // time it would report the service as old as the machine.
        let out = "ActiveEnterTimestampMonotonic=0\n";
        assert_eq!(parse_monotonic_timestamp(out, 1_785_000_000), None);

        let out = "ActiveEnterTimestampMonotonic=\n";
        assert_eq!(parse_monotonic_timestamp(out, 1_785_000_000), None);
    }

    #[test]
    fn boot_time_comes_from_btime() {
        let stat = "cpu  1 2 3 4 5 6 7\nintr 0\nctxt 123\nbtime 1768143387\nprocesses 9\n";
        assert_eq!(parse_btime(stat), Some(1768143387));

        // A kernel that does not report it leaves the fallback unusable, which
        // is reported as unknown rather than as an epoch near 1970.
        assert_eq!(parse_btime("cpu  1 2 3 4\nprocesses 9\n"), None);
    }

    #[test]
    fn uptime_is_unknown_without_a_start_time() {
        let data = SystemData::default();
        assert_eq!(data.uptime_since_restart(), "...");
    }

    #[test]
    fn cpu_times_come_from_the_aggregate_line() {
        // user nice system idle iowait irq softirq steal guest guest_nice
        let stat = "cpu  100 5 50 800 40 3 2 10 7 1\ncpu0 1 2 3 4 5 6 7 8\n";
        let times = parse_cpu_times(stat).unwrap();
        // idle + iowait: waiting on storage is not working.
        assert_eq!(times.idle, 840);
        // The first eight fields only: guest time is already inside user and
        // nice, so including fields nine and ten would count it twice.
        assert_eq!(times.total, 1010);

        assert_eq!(parse_cpu_times("intr 0 1 2\n"), None);
        assert_eq!(parse_cpu_times("cpu  1 2 3\n"), None);
    }

    #[test]
    fn cpu_percentage_is_the_rate_between_two_readings() {
        let prev = CpuTimes {
            idle: 800,
            total: 1000,
        };
        // 500 elapsed, 400 of it idle: 20% busy over the window, regardless of
        // what the counters accumulated before it.
        let now = CpuTimes {
            idle: 1200,
            total: 1500,
        };
        assert_eq!(cpu_percentage(prev, now), Some(20.0));
    }

    #[test]
    fn an_unusable_pair_of_readings_reports_nothing() {
        let base = CpuTimes {
            idle: 800,
            total: 1000,
        };
        // No time elapsed between reads.
        assert_eq!(cpu_percentage(base, base), None);
        // Counters that went backwards are not comparable.
        assert_eq!(
            cpu_percentage(
                base,
                CpuTimes {
                    idle: 700,
                    total: 900
                }
            ),
            None
        );
    }

    #[test]
    fn the_difference_is_signed_against_the_external_height() {
        let data = SystemData {
            external_block: Some(1000),
            ..Default::default()
        };
        assert_eq!(data.block_difference(1000), Some(0));
        assert_eq!(data.block_difference(994), Some(6));
        assert_eq!(data.block_difference(1003), Some(-3));
    }

    #[test]
    fn without_an_external_height_there_is_no_difference_to_report() {
        let data = SystemData::default();
        assert_eq!(data.external_block, None);
        assert_eq!(data.block_difference(1000), None);
    }

    #[test]
    fn a_block_number_is_read_with_or_without_the_prefix() {
        assert_eq!(parse_hex_block("0x3e8").unwrap(), 1000);
        assert_eq!(parse_hex_block("3e8").unwrap(), 1000);
        assert_eq!(parse_hex_block("0x0").unwrap(), 0);
    }

    #[test]
    fn an_unreadable_block_number_is_refused_rather_than_zeroed() {
        assert!(parse_hex_block("0x").is_err());
        assert!(parse_hex_block("latest").is_err());
        assert!(parse_hex_block("").is_err());
    }
}
