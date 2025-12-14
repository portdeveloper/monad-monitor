use anyhow::Result;
use reqwest::Client;
use serde_json::json;
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

    // Consensus info
    pub epoch: u64,
    pub round: u64,
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

    pub fn verified_lag(&self) -> u64 {
        self.history_latest.saturating_sub(self.latest_verified)
    }

    pub fn all_services_running(&self) -> bool {
        self.service_bft && self.service_execution && self.service_rpc
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
        }

        // Fetch external block number
        if let Ok(block) = self.fetch_external_block().await {
            data.external_block = block;
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

fn fetch_services_status() -> (bool, bool, bool) {
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

    (bft, execution, rpc)
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
