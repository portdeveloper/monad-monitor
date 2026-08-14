use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// A node that stops answering must not park the subscription: if nothing
/// arrives within this window the connection is treated as dead and retried.
/// New heads land roughly twice a second, so this leaves a wide margin.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The opening requests either come back promptly or the endpoint is not
/// usable, and waiting forever on them hides the problem.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect delay, doubling up to the ceiling. A node that is down stays down
/// for a while, and retrying in a tight loop only adds load to a host that is
/// already having a bad time.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub struct Block {
    pub number: u64,
    pub hash: String,
    pub tx_count: usize,
    pub timestamp: u64,
    pub gas_used: u64,
    pub gas_limit: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RpcData {
    pub block_number: u64,
    pub gas_price_gwei: f64,
    pub recent_blocks: Vec<Block>,
    pub client_version: String,
}

/// What the subscription reports back. The stream going away is news in its own
/// right: without it a dead connection is indistinguishable from a quiet one.
#[derive(Debug, Clone)]
pub enum RpcEvent {
    Data(RpcData),
    Connected,
    Disconnected(String),
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
    pub fn subscribe(&self, tx: mpsc::Sender<RpcEvent>) -> tokio::task::JoinHandle<()> {
        let endpoint = self.endpoint.clone();

        tokio::spawn(async move {
            let mut backoff = RECONNECT_MIN;

            loop {
                // Every way out of a subscription is a disconnect, including
                // the clean ones: a server-side close ends the stream just as
                // surely as a transport error does.
                let reason = match run_subscription(&endpoint, &tx).await {
                    Ok(streamed) => {
                        // Reaching the stream means the endpoint is healthy, so
                        // the next retry starts from the short delay again.
                        if streamed {
                            backoff = RECONNECT_MIN;
                        }
                        "connection closed by the node".to_string()
                    }
                    Err(e) => format!("{:#}", e),
                };
                let _ = tx.send(RpcEvent::Disconnected(reason)).await;

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        })
    }

    /// Fetch a single snapshot of RPC data (block number, gas price, client
    /// version) and return, without subscribing. Used by the headless JSON
    /// mode where we want one reading rather than a live stream.
    pub async fn fetch_once(&self) -> Result<RpcData> {
        let (ws_stream, _) = connect_async(&self.endpoint)
            .await
            .with_context(|| format!("Failed to connect to {}", self.endpoint))?;
        let (mut write, mut read) = ws_stream.split();

        let mut data = RpcData::default();
        let requests = vec![
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
        for req in &requests {
            write
                .send(Message::Text(serde_json::to_string(req)?))
                .await?;
        }

        let mut responses: HashMap<u32, Value> = HashMap::new();
        while responses.len() < requests.len() {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                        if let (Some(id), Some(result)) = (resp.id, resp.result) {
                            responses.insert(id, result);
                        }
                    }
                }
                // Connection closed or errored before all replies arrived: return
                // whatever we have rather than blocking forever.
                _ => break,
            }
        }

        if let Some(hex) = responses.get(&0).and_then(|v| v.as_str()) {
            data.block_number = parse_hex_u64(hex);
        }
        if let Some(hex) = responses.get(&1).and_then(|v| v.as_str()) {
            data.gas_price_gwei = parse_hex_u64(hex) as f64 / 1_000_000_000.0;
        }
        if let Some(version) = responses.get(&2).and_then(|v| v.as_str()) {
            data.client_version = version.to_string();
        }

        Ok(data)
    }
}

/// Runs one connection until it ends. `Ok(true)` means the subscription was
/// live and streaming before it dropped, which is what tells the caller the
/// endpoint itself is fine.
async fn run_subscription(endpoint: &str, tx: &mpsc::Sender<RpcEvent>) -> Result<bool> {
    let (ws_stream, _) = connect_async(endpoint)
        .await
        .with_context(|| format!("Failed to connect to {}", endpoint))?;

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

    // Collect initial responses. A node that answers some of these and then
    // goes quiet used to leave this loop waiting forever; the timeout turns
    // that into a reconnect instead.
    let mut responses: HashMap<u32, Value> = HashMap::new();
    let mut received = 0;
    while received < 3 {
        if let Message::Text(text) = read_message(&mut read, HANDSHAKE_TIMEOUT).await? {
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
    let _ = tx.send(RpcEvent::Data(data.clone())).await;

    // Subscribe to new block headers
    let subscribe_req = JsonRpcRequest {
        jsonrpc: "2.0",
        method: "eth_subscribe".to_string(),
        params: json!(["newHeads"]),
        id: 999,
    };
    write
        .send(Message::Text(serde_json::to_string(&subscribe_req)?))
        .await?;

    // Past this point the connection is established and streaming, which is
    // what the caller needs to know to reset its reconnect delay.
    let _ = tx.send(RpcEvent::Connected).await;

    // Process incoming messages
    loop {
        let msg = read_message(&mut read, READ_TIMEOUT).await?;
        match msg {
            Message::Text(text) => {
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
                                // Use block number as request id to match response to correct block
                                let hex_num = format!("0x{:x}", number);
                                let block_req = JsonRpcRequest {
                                    jsonrpc: "2.0",
                                    method: "eth_getBlockByNumber".to_string(),
                                    params: json!([hex_num, false]),
                                    id: (number % 100000) as u32 + 10000,
                                };
                                write
                                    .send(Message::Text(serde_json::to_string(&block_req)?))
                                    .await?;

                                // Also fetch gas price periodically
                                let gas_req = JsonRpcRequest {
                                    jsonrpc: "2.0",
                                    method: "eth_gasPrice".to_string(),
                                    params: json!([]),
                                    id: 1001,
                                };
                                write
                                    .send(Message::Text(serde_json::to_string(&gas_req)?))
                                    .await?;

                                // Send update immediately
                                let _ = tx.send(RpcEvent::Data(data.clone())).await;
                            }
                        }
                    } else if let (Some(id), Some(result)) = (resp.id, resp.result) {
                        // Handle response to our requests
                        if id >= 10000 && id < 110000 {
                            // Block details response - update tx count for matching block
                            let block_num_suffix = (id - 10000) as u64;
                            let tx_count = result["transactions"]
                                .as_array()
                                .map(|arr| arr.len())
                                .unwrap_or(0);
                            // Find the block with matching number suffix
                            if let Some(block) = data
                                .recent_blocks
                                .iter_mut()
                                .find(|b| b.number % 100000 == block_num_suffix)
                            {
                                block.tx_count = tx_count;
                            }
                            let _ = tx.send(RpcEvent::Data(data.clone())).await;
                        } else if id == 1001 {
                            // Gas price response
                            if let Some(hex) = result.as_str() {
                                data.gas_price_gwei = parse_hex_u64(hex) as f64 / 1_000_000_000.0;
                            }
                        }
                    }
                }
            }
            Message::Close(_) => break,
            // Anything else (a ping, a binary frame) is still traffic, so it
            // counts as the connection being alive and resets the read window.
            _ => {}
        }
    }

    Ok(true)
}

/// Reads one frame, or fails. Every wait on the socket goes through here so a
/// silent node surfaces as an error instead of parking the task.
async fn read_message<R>(read: &mut R, limit: Duration) -> Result<Message>
where
    R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    match tokio::time::timeout(limit, read.next()).await {
        Err(_) => Err(anyhow!(
            "no response from the node for {}s",
            limit.as_secs()
        )),
        Ok(None) => Err(anyhow!("connection closed")),
        Ok(Some(Err(e))) => Err(e).context("websocket read failed"),
        Ok(Some(Ok(msg))) => Ok(msg),
    }
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
    // Send all block requests. Only a request that actually went out gets a
    // reply, so the expected count follows the sends: waiting on a request
    // that failed to send is waiting on a reply that was never asked for.
    let mut expected = 0;
    for i in 0..count {
        let block_num = start_block.saturating_sub(i as u64);
        let hex_num = format!("0x{:x}", block_num);
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "eth_getBlockByNumber".to_string(),
            params: json!([hex_num, false]),
            id: 100 + i,
        };
        if write
            .send(Message::Text(serde_json::to_string(&req)?))
            .await
            .is_ok()
        {
            expected += 1;
        }
    }

    // Collect responses. One request going unanswered used to hang the whole
    // subscription here, so this waits on the same bounded read as everything
    // else and gives up to the reconnect path if the node stops replying.
    let mut block_responses: HashMap<u32, Value> = HashMap::new();
    let mut received = 0;
    while received < expected {
        if let Message::Text(text) = read_message(read, HANDSHAKE_TIMEOUT).await? {
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                if let Some(id) = resp.id {
                    if (100..100 + count).contains(&id) {
                        // A null result is still an answer: the node has no
                        // such block (pruned history, or a chain shorter than
                        // the window). The block is skipped, not waited for
                        // again — treating it as unanswered is what used to
                        // tear the whole connection down and retry forever.
                        if let Some(result) = resp.result {
                            block_responses.insert(id, result);
                        }
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
                timestamp: result["timestamp"].as_str().map(parse_hex_u64).unwrap_or(0),
                gas_used: result["gasUsed"].as_str().map(parse_hex_u64).unwrap_or(0),
                gas_limit: result["gasLimit"].as_str().map(parse_hex_u64).unwrap_or(0),
            });
        }
    }

    Ok(blocks)
}

fn parse_hex_u64(hex: &str) -> u64 {
    let hex = hex.trim_start_matches("0x");
    u64::from_str_radix(hex, 16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};

    /// A sink that accepts the first `succeed` sends and refuses the rest, so
    /// a test can drive `fetch_blocks` through send failures without a socket.
    struct MockSink {
        sent: usize,
        succeed: usize,
    }

    impl MockSink {
        fn accepting_all() -> Self {
            Self {
                sent: 0,
                succeed: usize::MAX,
            }
        }

        fn accepting(succeed: usize) -> Self {
            Self { sent: 0, succeed }
        }
    }

    impl futures::Sink<Message> for MockSink {
        type Error = std::io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(mut self: Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
            let index = self.sent;
            self.sent += 1;
            if index < self.succeed {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "send failed",
                ))
            }
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// The wire messages a node would send back, as a finished stream. If the
    /// code under test tries to read past the end, `read_message` reports the
    /// connection closed and the test fails, which is exactly the regression
    /// being guarded against: waiting for replies that are never coming.
    fn replies(
        texts: Vec<String>,
    ) -> impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin {
        futures::stream::iter(
            texts
                .into_iter()
                .map(|t| Ok::<_, tokio_tungstenite::tungstenite::Error>(Message::Text(t))),
        )
    }

    fn block_reply(id: u32) -> String {
        format!(
            r#"{{"id":{},"result":{{"hash":"0xabc","transactions":["0x1","0x2"],"timestamp":"0x0","gasUsed":"0x5","gasLimit":"0x64"}}}}"#,
            id
        )
    }

    fn null_reply(id: u32) -> String {
        format!(r#"{{"id":{},"result":null}}"#, id)
    }

    #[tokio::test]
    async fn a_null_result_counts_as_answered_and_the_block_is_skipped() {
        let mut write = MockSink::accepting_all();
        // Blocks 200 and 198 exist; 199 is pruned and answers null. The stream
        // holds exactly three replies, so completing at all proves nothing
        // waited on a fourth.
        let mut read = replies(vec![block_reply(100), null_reply(101), block_reply(102)]);

        let blocks = fetch_blocks(&mut write, &mut read, 200, 3).await.unwrap();

        let numbers: Vec<u64> = blocks.iter().map(|b| b.number).collect();
        assert_eq!(numbers, vec![200, 198]);
    }

    #[tokio::test]
    async fn every_block_missing_still_comes_back_rather_than_erroring() {
        // A freshly started chain can be shorter than the whole window. The
        // backfill returns empty and the caller carries on to the
        // subscription, instead of tearing the connection down.
        let mut write = MockSink::accepting_all();
        let mut read = replies(vec![null_reply(100), null_reply(101), null_reply(102)]);

        let blocks = fetch_blocks(&mut write, &mut read, 2, 3).await.unwrap();
        assert!(blocks.is_empty());
    }

    #[tokio::test]
    async fn a_failed_send_is_not_waited_for() {
        // Only the first request goes out, so only one reply exists. Expecting
        // three would leave the loop reading a stream with nothing left.
        let mut write = MockSink::accepting(1);
        let mut read = replies(vec![block_reply(100)]);

        let blocks = fetch_blocks(&mut write, &mut read, 200, 3).await.unwrap();

        let numbers: Vec<u64> = blocks.iter().map(|b| b.number).collect();
        assert_eq!(numbers, vec![200]);
    }

    #[tokio::test]
    async fn replies_out_of_order_and_interleaved_noise_still_land() {
        // Subscription notifications and unrelated ids arrive mixed into the
        // backfill replies on a real socket; none of them may count toward the
        // expected total or the loop finishes early.
        let mut write = MockSink::accepting_all();
        let mut read = replies(vec![
            r#"{"method":"eth_subscription","params":{"result":{}}}"#.to_string(),
            block_reply(101),
            r#"{"id":999,"result":"0xdeadbeef"}"#.to_string(),
            null_reply(100),
        ]);

        let blocks = fetch_blocks(&mut write, &mut read, 200, 2).await.unwrap();

        let numbers: Vec<u64> = blocks.iter().map(|b| b.number).collect();
        assert_eq!(numbers, vec![199]);
        assert_eq!(blocks[0].tx_count, 2);
    }
}
