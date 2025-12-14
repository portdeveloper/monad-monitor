use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use std::fs;
use std::process::Command;

/// Data from system commands (monad-mpt, systemctl, external RPC)
#[derive(Debug, Clone, Default)]
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

    // External block for comparison
    pub external_block: u64,

    // System resources
    pub memory_used_pct: f64,
    pub memory_used_gb: f64,
    pub memory_total_gb: f64,
    pub cpu_usage_pct: f64,

    // Network (bytes since boot, for calculating rate)
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,

    // Node identifier (hostname)
    pub node_id: String,

    // Service start time (seconds since epoch)
    pub service_started_at: u64,
}

impl SystemData {
    pub fn block_difference(&self, local_block: u64) -> i64 {
        if self.external_block == 0 {
            0
        } else {
            self.external_block as i64 - local_block as i64
        }
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

pub struct SystemClient {
    http_client: Client,
    network: String,
}

impl SystemClient {
    pub fn new(network: &str) -> Self {
        Self {
            http_client: Client::new(),
            network: network.to_string(),
        }
    }

    pub async fn fetch(&self) -> Result<SystemData> {
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

        // Fetch external block number
        if let Ok(block) = self.fetch_external_block().await {
            data.external_block = block;
        }

        // Fetch system resources (blocking, but fast)
        if let Ok(resources) = tokio::task::spawn_blocking(fetch_system_resources).await {
            data.memory_used_pct = resources.0;
            data.memory_used_gb = resources.1;
            data.memory_total_gb = resources.2;
            data.cpu_usage_pct = resources.3;
            data.net_rx_bytes = resources.4;
            data.net_tx_bytes = resources.5;
        }

        // Fetch hostname
        if let Ok(hostname) = fs::read_to_string("/etc/hostname") {
            data.node_id = hostname.trim().to_string();
        }

        Ok(data)
    }

    async fn fetch_external_block(&self) -> Result<u64> {
        let url = format!("https://rpc-{}.monadinfra.com", self.network);
        let response = self
            .http_client
            .post(&url)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "eth_blockNumber",
                "params": [],
                "id": 1
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        if let Some(hex) = response["result"].as_str() {
            let hex = hex.trim_start_matches("0x");
            return Ok(u64::from_str_radix(hex, 16).unwrap_or(0));
        }
        Ok(0)
    }
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

    // Get service start time from monad-bft (parse ActiveEnterTimestamp)
    let started_at = Command::new("systemctl")
        .args(["show", "monad-bft", "--property=ActiveEnterTimestamp"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| parse_systemd_timestamp(&s))
        .unwrap_or(0);

    (bft, execution, rpc, started_at)
}

/// Parse systemd timestamp like "ActiveEnterTimestamp=Thu 2025-12-11 21:20:59 CET"
fn parse_systemd_timestamp(output: &str) -> Option<u64> {
    // Extract the timestamp part after "="
    let ts_str = output.split('=').nth(1)?.trim();
    if ts_str.is_empty() || ts_str == "n/a" {
        return None;
    }

    // Parse format: "Thu 2025-12-11 21:20:59 CET"
    // Skip day name, parse date and time
    let parts: Vec<&str> = ts_str.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }

    let date = parts[1]; // "2025-12-11"
    let time = parts[2]; // "21:20:59"

    // Parse date
    let date_parts: Vec<u32> = date.split('-').filter_map(|s| s.parse().ok()).collect();
    if date_parts.len() != 3 {
        return None;
    }

    // Parse time
    let time_parts: Vec<u32> = time.split(':').filter_map(|s| s.parse().ok()).collect();
    if time_parts.len() != 3 {
        return None;
    }

    // Simple approximation - convert to seconds since epoch
    // This is a rough calculation but good enough for uptime display
    let year = date_parts[0] as u64;
    let month = date_parts[1] as u64;
    let day = date_parts[2] as u64;
    let hour = time_parts[0] as u64;
    let min = time_parts[1] as u64;
    let sec = time_parts[2] as u64;

    // Days since epoch (Jan 1, 1970), simplified
    let years_since_1970 = year.saturating_sub(1970);
    let leap_years = (years_since_1970 + 1) / 4; // rough estimate
    let days_in_prev_months: u64 = match month {
        1 => 0,
        2 => 31,
        3 => 59,
        4 => 90,
        5 => 120,
        6 => 151,
        7 => 181,
        8 => 212,
        9 => 243,
        10 => 273,
        11 => 304,
        12 => 334,
        _ => 0,
    };

    let total_days = years_since_1970 * 365 + leap_years + days_in_prev_months + day - 1;
    let total_secs = total_days * 86400 + hour * 3600 + min * 60 + sec;

    // Adjust for timezone (assume CET = UTC+1, subtract 1 hour)
    Some(total_secs.saturating_sub(3600))
}

/// Returns (mem_pct, mem_used_gb, mem_total_gb, cpu_pct, net_rx, net_tx)
fn fetch_system_resources() -> (f64, f64, f64, f64, u64, u64) {
    let mut mem_pct = 0.0;
    let mut mem_used_gb = 0.0;
    let mut mem_total_gb = 0.0;
    let mut cpu_pct = 0.0;
    let mut net_rx: u64 = 0;
    let mut net_tx: u64 = 0;

    // Parse /proc/meminfo for memory
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        let mut total_kb: u64 = 0;
        let mut available_kb: u64 = 0;

        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                total_kb = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            } else if line.starts_with("MemAvailable:") {
                available_kb = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
        }

        if total_kb > 0 {
            let used_kb = total_kb.saturating_sub(available_kb);
            mem_total_gb = total_kb as f64 / 1024.0 / 1024.0;
            mem_used_gb = used_kb as f64 / 1024.0 / 1024.0;
            mem_pct = (used_kb as f64 / total_kb as f64) * 100.0;
        }
    }

    // Parse /proc/stat for CPU (simplified - just idle percentage)
    if let Ok(stat) = fs::read_to_string("/proc/stat") {
        if let Some(cpu_line) = stat.lines().next() {
            let parts: Vec<u64> = cpu_line
                .split_whitespace()
                .skip(1) // skip "cpu"
                .filter_map(|s| s.parse().ok())
                .collect();

            if parts.len() >= 4 {
                let total: u64 = parts.iter().sum();
                let idle = parts.get(3).unwrap_or(&0);
                if total > 0 {
                    cpu_pct = 100.0 - (*idle as f64 / total as f64 * 100.0);
                }
            }
        }
    }

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

    (mem_pct, mem_used_gb, mem_total_gb, cpu_pct, net_rx, net_tx)
}

fn parse_mpt_output(output: &str, data: &mut SystemData) {
    for line in output.lines() {
        let line = line.trim();

        // Parse disk capacity/used line: "1.75 Tb      109.30 Gb  6.11%"
        if line.contains("Tb") && line.contains("Gb") && line.contains('%') {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                // Capacity
                if let Ok(cap) = parts[0].parse::<f64>() {
                    let unit = parts[1];
                    data.disk_capacity_gb = match unit {
                        "Tb" => cap * 1024.0,
                        "Gb" => cap,
                        _ => cap,
                    };
                }
                // Used
                if let Ok(used) = parts[2].parse::<f64>() {
                    let unit = parts[3];
                    data.disk_used_gb = match unit {
                        "Tb" => used * 1024.0,
                        "Gb" => used,
                        "Mb" => used / 1024.0,
                        _ => used,
                    };
                }
                // Percentage
                if let Ok(pct) = parts[4].trim_end_matches('%').parse::<f64>() {
                    data.disk_used_pct = pct;
                }
            }
        }

        // Parse history line: "MPT database has 637751 history, earliest is 41295350 latest is 41933100."
        if line.contains("MPT database has") && line.contains("history") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for (i, part) in parts.iter().enumerate() {
                if *part == "has" && i + 1 < parts.len() {
                    data.history_count = parts[i + 1].parse().unwrap_or(0);
                }
                if *part == "earliest" && i + 2 < parts.len() {
                    data.history_earliest = parts[i + 2].parse().unwrap_or(0);
                }
                if *part == "latest" && i + 2 < parts.len() {
                    // Remove trailing period
                    let val = parts[i + 2].trim_end_matches('.');
                    data.history_latest = val.parse().unwrap_or(0);
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
