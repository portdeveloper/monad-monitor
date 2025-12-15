use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

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

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: String,
    params: Value,
    id: u32,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    id: Option<u32>,
    result: Option<Value>,
    method: Option<String>,
    params: Option<SubscriptionParams>,
}

#[derive(Deserialize)]
struct SubscriptionParams {
    result: Value,
}

pub struct RpcClient {
    endpoint: String,
}

impl RpcClient {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
        }
    }

    /// Spawn a background task that subscribes to new blocks and sends updates
    pub fn subscribe(
        &self,
        tx: mpsc::Sender<RpcData>,
    ) -> tokio::task::JoinHandle<()> {
        let endpoint = self.endpoint.clone();

        tokio::spawn(async move {
            loop {
                if let Err(_) = run_subscription(&endpoint, &tx).await {
                    // Reconnect after a brief delay on error
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        })
    }
}

async fn run_subscription(endpoint: &str, tx: &mpsc::Sender<RpcData>) -> Result<()> {
    let (ws_stream, _) = connect_async(endpoint)
        .await
        .context("Failed to connect to WebSocket")?;

    let (mut write, mut read) = ws_stream.split();

    // Get initial data
    let mut data = RpcData::default();

    // Send initial requests
    let initial_requests = vec![
        JsonRpcRequest {
            jsonrpc: "2.0",
            method: "eth_blockNumber".to_string(),
            params: json!([]),
            id: 0,
        },
        JsonRpcRequest {
            jsonrpc: "2.0",
            method: "eth_gasPrice".to_string(),
            params: json!([]),
            id: 1,
        },
        JsonRpcRequest {
            jsonrpc: "2.0",
            method: "web3_clientVersion".to_string(),
            params: json!([]),
            id: 2,
        },
    ];

    for req in &initial_requests {
        let text = serde_json::to_string(req)?;
        write.send(Message::Text(text)).await?;
    }

    // Collect initial responses
    let mut responses: HashMap<u32, Value> = HashMap::new();
    let mut received = 0;
    while received < 3 {
        if let Some(Ok(Message::Text(text))) = read.next().await {
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                if let (Some(id), Some(result)) = (resp.id, resp.result) {
                    responses.insert(id, result);
                    received += 1;
                }
            }
        }
    }

    // Parse initial data
    if let Some(result) = responses.get(&0) {
        if let Some(hex) = result.as_str() {
            data.block_number = parse_hex_u64(hex);
        }
    }
    if let Some(result) = responses.get(&1) {
        if let Some(hex) = result.as_str() {
            data.gas_price_gwei = parse_hex_u64(hex) as f64 / 1_000_000_000.0;
        }
    }
    if let Some(result) = responses.get(&2) {
        if let Some(version) = result.as_str() {
            data.client_version = version.to_string();
        }
    }

    // Fetch initial blocks
    if data.block_number > 0 {
        data.recent_blocks = fetch_blocks(&mut write, &mut read, data.block_number, 30).await?;
    }

    // Send initial data
    let _ = tx.send(data.clone()).await;

    // Subscribe to new block headers
    let subscribe_req = JsonRpcRequest {
        jsonrpc: "2.0",
        method: "eth_subscribe".to_string(),
        params: json!(["newHeads"]),
        id: 999,
    };
    write.send(Message::Text(serde_json::to_string(&subscribe_req)?)).await?;

    // Process incoming messages
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                    // Check if this is a subscription notification
                    if resp.method.as_deref() == Some("eth_subscription") {
                        if let Some(params) = resp.params {
                            let block_data = &params.result;

                            // Parse the new block header
                            let number = block_data["number"]
                                .as_str()
                                .map(parse_hex_u64)
                                .unwrap_or(0);

                            if number > 0 {
                                let new_block = Block {
                                    number,
                                    hash: block_data["hash"].as_str().unwrap_or("0x0").to_string(),
                                    tx_count: 0, // Headers don't include tx count, will update below
                                    timestamp: block_data["timestamp"]
                                        .as_str()
                                        .map(parse_hex_u64)
                                        .unwrap_or(0),
                                    gas_used: block_data["gasUsed"]
                                        .as_str()
                                        .map(parse_hex_u64)
                                        .unwrap_or(0),
                                    gas_limit: block_data["gasLimit"]
                                        .as_str()
                                        .map(parse_hex_u64)
                                        .unwrap_or(0),
                                };

                                // Update data
                                data.block_number = number;

                                // Add new block to front, keep max 30
                                data.recent_blocks.insert(0, new_block);
                                if data.recent_blocks.len() > 30 {
                                    data.recent_blocks.pop();
                                }

                                // Fetch full block to get tx count
                                let hex_num = format!("0x{:x}", number);
                                let block_req = JsonRpcRequest {
                                    jsonrpc: "2.0",
                                    method: "eth_getBlockByNumber".to_string(),
                                    params: json!([hex_num, false]),
                                    id: 1000,
                                };
                                write.send(Message::Text(serde_json::to_string(&block_req)?)).await?;

                                // Also fetch gas price periodically
                                let gas_req = JsonRpcRequest {
                                    jsonrpc: "2.0",
                                    method: "eth_gasPrice".to_string(),
                                    params: json!([]),
                                    id: 1001,
                                };
                                write.send(Message::Text(serde_json::to_string(&gas_req)?)).await?;

                                // Send update immediately
                                let _ = tx.send(data.clone()).await;
                            }
                        }
                    } else if let (Some(id), Some(result)) = (resp.id, resp.result) {
                        // Handle response to our requests
                        if id == 1000 {
                            // Block details response - update tx count
                            let tx_count = result["transactions"]
                                .as_array()
                                .map(|arr| arr.len())
                                .unwrap_or(0);
                            if let Some(block) = data.recent_blocks.first_mut() {
                                block.tx_count = tx_count;
                            }
                            let _ = tx.send(data.clone()).await;
                        } else if id == 1001 {
                            // Gas price response
                            if let Some(hex) = result.as_str() {
                                data.gas_price_gwei = parse_hex_u64(hex) as f64 / 1_000_000_000.0;
                            }
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Err(_) => break,
            _ => {}
        }
    }

    Ok(())
}

async fn fetch_blocks<S, R>(
    write: &mut S,
    read: &mut R,
    start_block: u64,
    count: u32,
) -> Result<Vec<Block>>
where
    S: SinkExt<Message> + Unpin,
    R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    <S as futures::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    // Send all block requests
    for i in 0..count {
        let block_num = start_block.saturating_sub(i as u64);
        let hex_num = format!("0x{:x}", block_num);
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "eth_getBlockByNumber".to_string(),
            params: json!([hex_num, false]),
            id: 100 + i,
        };
        write.send(Message::Text(serde_json::to_string(&req)?)).await.ok();
    }

    // Collect responses
    let mut block_responses: HashMap<u32, Value> = HashMap::new();
    let mut received = 0;
    while received < count {
        if let Some(Ok(Message::Text(text))) = read.next().await {
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                if let (Some(id), Some(result)) = (resp.id, resp.result) {
                    if id >= 100 && id < 100 + count {
                        block_responses.insert(id, result);
                        received += 1;
                    }
                }
            }
        }
    }

    // Parse blocks in order
    let mut blocks = Vec::with_capacity(count as usize);
    for i in 0..count {
        if let Some(result) = block_responses.get(&(100 + i)) {
            let block_num = start_block.saturating_sub(i as u64);
            blocks.push(Block {
                number: block_num,
                hash: result["hash"].as_str().unwrap_or("0x0").to_string(),
                tx_count: result["transactions"]
                    .as_array()
                    .map(|arr| arr.len())
                    .unwrap_or(0),
                timestamp: result["timestamp"]
                    .as_str()
                    .map(parse_hex_u64)
                    .unwrap_or(0),
                gas_used: result["gasUsed"]
                    .as_str()
                    .map(parse_hex_u64)
                    .unwrap_or(0),
                gas_limit: result["gasLimit"]
                    .as_str()
                    .map(parse_hex_u64)
                    .unwrap_or(0),
            });
        }
    }

    Ok(blocks)
}

fn parse_hex_u64(hex: &str) -> u64 {
    let hex = hex.trim_start_matches("0x");
    u64::from_str_radix(hex, 16).unwrap_or(0)
}
