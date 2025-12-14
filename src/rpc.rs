use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Block {
    pub number: u64,
    pub hash: String,
    pub tx_count: usize,
    pub timestamp: u64,
    pub gas_used: u64,
    pub gas_limit: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RpcData {
    pub block_number: u64,
    pub gas_price_gwei: f64,
    pub recent_blocks: Vec<Block>,
    pub client_version: String,
}

pub struct RpcClient {
    client: Client,
    endpoint: String,
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'a str,
    method: &'a str,
    params: Value,
    id: u32,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    result: Option<Value>,
    error: Option<Value>,
}

impl RpcClient {
    pub fn new(endpoint: &str) -> Self {
        Self {
            client: Client::new(),
            endpoint: endpoint.to_string(),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        };

        let response: JsonRpcResponse = self
            .client
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .context("RPC request failed")?
            .json()
            .await
            .context("Failed to parse RPC response")?;

        if let Some(error) = response.error {
            anyhow::bail!("RPC error: {}", error);
        }

        response.result.context("No result in RPC response")
    }

    pub async fn fetch(&self) -> Result<RpcData> {
        let mut data = RpcData::default();

        // Fetch block number
        if let Ok(result) = self.call("eth_blockNumber", json!([])).await {
            if let Some(hex) = result.as_str() {
                data.block_number = parse_hex_u64(hex);
            }
        }

        // Fetch gas price
        if let Ok(result) = self.call("eth_gasPrice", json!([])).await {
            if let Some(hex) = result.as_str() {
                let wei = parse_hex_u64(hex);
                data.gas_price_gwei = wei as f64 / 1_000_000_000.0;
            }
        }

        // Fetch client version
        if let Ok(result) = self.call("web3_clientVersion", json!([])).await {
            if let Some(version) = result.as_str() {
                data.client_version = version.to_string();
            }
        }

        // Fetch recent blocks (last 30 - UI will show as many as fit)
        if data.block_number > 0 {
            let mut blocks = Vec::with_capacity(30);
            for i in 0..30 {
                let block_num = data.block_number.saturating_sub(i);
                if let Ok(block) = self.get_block(block_num).await {
                    blocks.push(block);
                }
            }
            data.recent_blocks = blocks;
        }

        Ok(data)
    }

    async fn get_block(&self, number: u64) -> Result<Block> {
        let hex_num = format!("0x{:x}", number);
        let result = self
            .call("eth_getBlockByNumber", json!([hex_num, false]))
            .await?;

        let hash = result["hash"]
            .as_str()
            .unwrap_or("0x0")
            .to_string();

        let tx_count = result["transactions"]
            .as_array()
            .map(|arr| arr.len())
            .unwrap_or(0);

        let timestamp = result["timestamp"]
            .as_str()
            .map(parse_hex_u64)
            .unwrap_or(0);

        let gas_used = result["gasUsed"]
            .as_str()
            .map(parse_hex_u64)
            .unwrap_or(0);

        let gas_limit = result["gasLimit"]
            .as_str()
            .map(parse_hex_u64)
            .unwrap_or(0);

        Ok(Block {
            number,
            hash,
            tx_count,
            timestamp,
            gas_used,
            gas_limit,
        })
    }
}

fn parse_hex_u64(hex: &str) -> u64 {
    let hex = hex.trim_start_matches("0x");
    u64::from_str_radix(hex, 16).unwrap_or(0)
}
