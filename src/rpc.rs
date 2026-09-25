use alloy_primitives::U64;
use alloy_rpc_types_eth::{Block as AlloyBlock, Header as AlloyHeader};
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
/// usable, and waiting forever on them hides the problem. The same window
/// bounds the connect itself (TCP, TLS, upgrade): an endpoint that accepts
/// the socket and never answers the upgrade is not usable either.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect delay, doubling up to the ceiling. A node that is down stays down
/// for a while, and retrying in a tight loop only adds load to a host that is
/// already having a bad time.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Outstanding tracked requests tolerated before the subscription gives up and
/// reconnects. Each `newHeads` notification adds a block-detail and a
/// gas-price request; a node that keeps streaming heads but never answers them
/// would otherwise grow the map by two per head forever, and its
/// notifications keep resetting `READ_TIMEOUT` so the socket never looks
/// dead. 64 is many seconds of unanswered detail traffic on a healthy chain
/// and still a hard ceiling on a lying one.
const MAX_PENDING_REQUESTS: usize = 64;

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
    /// `None` while the quantity has never been read (or the opening reply
    /// was malformed). `Some(0)` is a real zero-height reading, which is
    /// different from "no reading yet".
    pub block_number: Option<u64>,
    /// `None` while the quantity has never been read. `Some(0.0)` is a real
    /// zero gas price, which is different from "no reading yet".
    pub gas_price_gwei: Option<f64>,
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

/// What an in-flight request id is waiting for. Matching a reply to its
/// request goes through this map, so a block-detail reply is tied to the full
/// block number that was asked for — never to a truncated suffix of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingRequest {
    BlockByNumber(u64),
    GasPrice,
    Subscribe,
}

/// Hands out unique request ids and remembers what each one is waiting for.
/// Ids are never reused while the map is live, so two concurrent
/// `eth_getBlockByNumber` calls — even for block numbers that collide under
/// `number % 100000` — cannot cross-patch each other's replies.
struct RequestTracker {
    next_id: u32,
    pending: HashMap<u32, PendingRequest>,
}

impl RequestTracker {
    /// Ids start high enough to stay clear of the fixed handshake ids
    /// (`0/1/2`) and the backfill range (`100..`) that run before the
    /// subscription enters its live loop.
    fn starting_at(first_id: u32) -> Self {
        Self {
            next_id: first_id,
            pending: HashMap::new(),
        }
    }

    fn next(&mut self, kind: PendingRequest) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.insert(id, kind);
        id
    }

    fn take(&mut self, id: u32) -> Option<PendingRequest> {
        self.pending.remove(&id)
    }

    fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Whether `more` further requests fit under the outstanding bound. The
    /// caller checks both detail and gas-price slots before sending either,
    /// so a silent node cannot leave a half-issued pair behind.
    fn will_fit(&self, more: usize) -> bool {
        self.pending_len() + more <= MAX_PENDING_REQUESTS
    }
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
        let mut received = 0;
        while received < requests.len() {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                        if let Some(id) = resp.id {
                            if (id as usize) < requests.len() {
                                // An error reply is an answer: that value stays
                                // at its default in the snapshot. Waiting for a
                                // result the node has declined would burn the
                                // whole timeout and report a healthy node
                                // unreachable.
                                if let Some(result) = resp.result {
                                    responses.insert(id, result);
                                }
                                received += 1;
                            }
                        }
                    }
                }
                // Connection closed or errored before all replies arrived: return
                // whatever we have rather than blocking forever.
                _ => break,
            }
        }

        // A malformed quantity leaves the field unknown (`None`) rather than
        // coercing to a measured zero, so consumers can tell "not read" from
        // a real zero-height block or a zero gas price.
        if let Some(n) = responses
            .get(&0)
            .and_then(|v| v.as_str())
            .and_then(parse_hex_u64)
        {
            data.block_number = Some(n);
        }
        if let Some(n) = responses
            .get(&1)
            .and_then(|v| v.as_str())
            .and_then(parse_hex_u64)
        {
            data.gas_price_gwei = Some(n as f64 / 1_000_000_000.0);
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
    run_subscription_with(endpoint, tx, HANDSHAKE_TIMEOUT).await
}

/// `run_subscription` with the connect deadline as a parameter, so a test does
/// not have to wait out `HANDSHAKE_TIMEOUT` to see a stalled connect fail.
async fn run_subscription_with(
    endpoint: &str,
    tx: &mpsc::Sender<RpcEvent>,
    connect_timeout: Duration,
) -> Result<bool> {
    // The reads after the upgrade have deadlines; the connect itself did not,
    // so an endpoint that accepts the socket and never answers the upgrade
    // parked this task for good and the caller's backoff loop never ran.
    let (ws_stream, _) = tokio::time::timeout(connect_timeout, connect_async(endpoint))
        .await
        .map_err(|_| {
            anyhow!(
                "connecting to {} timed out after {}s",
                endpoint,
                connect_timeout.as_secs_f32()
            )
        })?
        .with_context(|| format!("Failed to connect to {}", endpoint))?;

    let (mut write, mut read) = ws_stream.split();

    // Get initial data
    let mut data = RpcData::default();

    // Send initial requests. Handshake ids stay fixed at 0/1/2; they are
    // drained by `collect_replies` before the live loop starts and are never
    // concurrent with the tracked ids handed out below.
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

    // Collect the handshake replies. A node that answers some of these and
    // then goes quiet used to leave this loop waiting forever; the timeout
    // turns that into a reconnect instead. An error reply counts as answered —
    // a node that declines one of these calls can still stream blocks, and
    // that is the job.
    let responses = collect_replies(&mut read, initial_requests.len() as u32).await?;

    // Parse initial data. A quantity the node sent as malformed hex leaves the
    // field unknown instead of becoming a measured zero; a previously measured
    // value cannot exist yet because `data` starts at the default.
    if let Some(n) = responses
        .get(&0)
        .and_then(|v| v.as_str())
        .and_then(parse_hex_u64)
    {
        data.block_number = Some(n);
    }
    if let Some(n) = responses
        .get(&1)
        .and_then(|v| v.as_str())
        .and_then(parse_hex_u64)
    {
        data.gas_price_gwei = Some(n as f64 / 1_000_000_000.0);
    }
    if let Some(result) = responses.get(&2) {
        if let Some(version) = result.as_str() {
            data.client_version = version.to_string();
        }
    }

    // Fetch initial blocks. A height of zero is a reading, but there is
    // nothing behind it to backfill; only a height above genesis is worth
    // asking for history.
    if let Some(start) = data.block_number.filter(|&n| n > 0) {
        data.recent_blocks = fetch_blocks(&mut write, &mut read, start, 30).await?;
    }

    // Send initial data
    let _ = tx.send(RpcEvent::Data(data.clone())).await;

    // Ids handed out from here on are unique per request and carry the full
    // block number they are waiting for, so replies cannot land on the wrong
    // row even when two block numbers share a `number % 100000` suffix.
    let mut tracker = RequestTracker::starting_at(1000);

    // Subscribe to new block headers
    let subscribe_id = tracker.next(PendingRequest::Subscribe);
    let subscribe_req = JsonRpcRequest {
        jsonrpc: "2.0",
        method: "eth_subscribe".to_string(),
        params: json!(["newHeads"]),
        id: subscribe_id,
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
                            // A header that does not deserialize — bad hex in
                            // `number`, a missing field, a structurally wrong
                            // shape — is skipped entirely. The subscription
                            // stays up; no `Block` that looks measured is
                            // invented from unwrap_or defaults.
                            if let Some(new_block) = parse_new_head(&params.result) {
                                let number = new_block.number;

                                // Update data
                                data.block_number = Some(number);

                                // Add new block to front, keep max 30
                                data.recent_blocks.insert(0, new_block);
                                if data.recent_blocks.len() > 30 {
                                    data.recent_blocks.pop();
                                }

                                // Bound outstanding requests before issuing
                                // either half of this head's pair. A node that
                                // keeps streaming heads but never answers the
                                // detail/gas requests would grow the map by
                                // two per head while its notifications reset
                                // the read timeout; exceeding the bound ends
                                // the subscription so the caller reconnects.
                                if !tracker.will_fit(2) {
                                    return Err(anyhow!(
                                        "node left {} RPC requests unanswered; reconnecting",
                                        tracker.pending_len()
                                    ));
                                }

                                // Fetch full block to get tx count. The id
                                // carries the full block number so the reply
                                // patches the right row.
                                let block_id = tracker.next(PendingRequest::BlockByNumber(number));
                                let hex_num = format!("0x{:x}", number);
                                let block_req = JsonRpcRequest {
                                    jsonrpc: "2.0",
                                    method: "eth_getBlockByNumber".to_string(),
                                    params: json!([hex_num, false]),
                                    id: block_id,
                                };
                                write
                                    .send(Message::Text(serde_json::to_string(&block_req)?))
                                    .await?;

                                // Also fetch gas price periodically
                                let gas_id = tracker.next(PendingRequest::GasPrice);
                                let gas_req = JsonRpcRequest {
                                    jsonrpc: "2.0",
                                    method: "eth_gasPrice".to_string(),
                                    params: json!([]),
                                    id: gas_id,
                                };
                                write
                                    .send(Message::Text(serde_json::to_string(&gas_req)?))
                                    .await?;

                                // Send update immediately
                                let _ = tx.send(RpcEvent::Data(data.clone())).await;
                            }
                        }
                    } else if let Some(id) = resp.id {
                        match tracker.take(id) {
                            Some(PendingRequest::BlockByNumber(number)) => {
                                if apply_block_detail(&mut data, number, resp.result.as_ref()) {
                                    let _ = tx.send(RpcEvent::Data(data.clone())).await;
                                }
                            }
                            Some(PendingRequest::GasPrice) => {
                                if apply_gas_price(&mut data, resp.result.as_ref()) {
                                    let _ = tx.send(RpcEvent::Data(data.clone())).await;
                                }
                            }
                            // Subscribe ack, handshake ids, or anything we are
                            // no longer waiting on: nothing to patch.
                            Some(PendingRequest::Subscribe) | None => {}
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

/// Turns a `newHeads` notification payload into a [`Block`], or `None` if the
/// header does not deserialize. Callers skip the block on `None` and keep the
/// subscription running — a bad header is never allowed to become a row that
/// looks measured.
fn parse_new_head(value: &Value) -> Option<Block> {
    let header: AlloyHeader = serde_json::from_value(value.clone()).ok()?;
    let number = header.number;
    if number == 0 {
        return None;
    }
    Some(Block {
        number,
        hash: header.hash.to_string(),
        tx_count: 0, // Headers don't include tx count, will update below
        timestamp: header.timestamp,
        gas_used: header.gas_used,
        gas_limit: header.gas_limit,
    })
}

/// Applies an `eth_getBlockByNumber` reply to the recent block with the full
/// number the request was made for. Returns whether `data` changed.
///
/// A null result, a body that fails [`decode_block_body`], or a block that has
/// already fallen out of the 30-row window all leave `tx_count` untouched
/// rather than writing a guessed zero over an unknown.
fn apply_block_detail(data: &mut RpcData, number: u64, result: Option<&Value>) -> bool {
    let Some(result) = result else {
        // Null result: the node has no such block. Nothing to patch.
        return false;
    };
    let Some(block) = decode_block_body(result, number) else {
        // Not a trustworthy answer to this request: leave the row alone.
        return false;
    };
    let tx_count = block.transactions.len();
    let Some(row) = data.recent_blocks.iter_mut().find(|b| b.number == number) else {
        return false;
    };
    row.tx_count = tx_count;
    true
}

/// Applies an `eth_gasPrice` reply. Returns whether `data` changed.
///
/// A null result or a malformed quantity leaves the previous reading — or
/// still-unknown `None` — in place, so a bad live reply cannot turn a
/// measured price into a measured zero (or erase one that never existed).
pub(crate) fn apply_gas_price(data: &mut RpcData, result: Option<&Value>) -> bool {
    let Some(hex) = result.and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(n) = parse_hex_u64(hex) else {
        return false;
    };
    data.gas_price_gwei = Some(n as f64 / 1_000_000_000.0);
    true
}

/// Decodes an `eth_getBlockByNumber` body as the answer to the request for
/// `requested`, or `None` when the body is not that answer.
///
/// Two shapes are refused even though they deserialize:
///
/// - A body with no `transactions` list. Alloy maps a missing field to
///   `BlockTransactions::Uncle`, whose length is zero, so a node that omits
///   the list would overwrite a real count with a guessed zero. An explicit
///   empty array is a real zero-height answer and stays valid.
/// - A header whose `number` differs from `requested`. Accepting it and then
///   substituting the requested number would stamp one block's hash and
///   fields onto another's row.
fn decode_block_body(result: &Value, requested: u64) -> Option<AlloyBlock> {
    if !matches!(result.get("transactions"), Some(Value::Array(_))) {
        return None;
    }
    let block: AlloyBlock = serde_json::from_value(result.clone()).ok()?;
    if block.header.number != requested {
        return None;
    }
    Some(block)
}

/// Collects one reply for each request id below `expected` and returns the
/// results by id.
///
/// A reply is counted whether it carries a result or an error: an error is
/// still an answer, and holding out for a value the node has declined to
/// produce is what used to park the whole subscription until the timeout tore
/// it down. An id that ends up absent from the map simply leaves its value at
/// the caller's default. Unrelated traffic (subscription notifications,
/// foreign ids) does not count toward the total.
async fn collect_replies<R>(read: &mut R, expected: u32) -> Result<HashMap<u32, Value>>
where
    R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut responses: HashMap<u32, Value> = HashMap::new();
    let mut received = 0;
    while received < expected {
        if let Message::Text(text) = read_message(read, HANDSHAKE_TIMEOUT).await? {
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                if let Some(id) = resp.id {
                    if id < expected {
                        if let Some(result) = resp.result {
                            responses.insert(id, result);
                        }
                        received += 1;
                    }
                }
            }
        }
    }
    Ok(responses)
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
    // Each sent id is recorded against the full block number it asked for, so
    // the reply is matched by identity rather than by reconstructing the
    // number from an id arithmetic scheme.
    let mut sent: Vec<(u32, u64)> = Vec::with_capacity(count as usize);
    for i in 0..count {
        let block_num = start_block.saturating_sub(i as u64);
        let hex_num = format!("0x{:x}", block_num);
        let id = 100 + i;
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "eth_getBlockByNumber".to_string(),
            params: json!([hex_num, false]),
            id,
        };
        if write
            .send(Message::Text(serde_json::to_string(&req)?))
            .await
            .is_ok()
        {
            sent.push((id, block_num));
        }
    }

    // Collect responses. One request going unanswered used to hang the whole
    // subscription here, so this waits on the same bounded read as everything
    // else and gives up to the reconnect path if the node stops replying.
    let expected = sent.len();
    let expected_ids: std::collections::HashSet<u32> = sent.iter().map(|(id, _)| *id).collect();
    let mut block_responses: HashMap<u32, Value> = HashMap::new();
    let mut received = 0;
    while received < expected {
        if let Message::Text(text) = read_message(read, HANDSHAKE_TIMEOUT).await? {
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                if let Some(id) = resp.id {
                    if expected_ids.contains(&id) {
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

    // Parse blocks in request order. A body that fails to deserialize, has no
    // transaction list, or belongs to a different block is skipped the same
    // way a null result is: no Block built from unwrap_or defaults, just an
    // absent row.
    let mut blocks = Vec::with_capacity(expected);
    for (id, block_num) in &sent {
        if let Some(result) = block_responses.get(id) {
            if let Some(block) = decode_block_body(result, *block_num) {
                blocks.push(Block {
                    number: *block_num,
                    hash: block.header.hash.to_string(),
                    tx_count: block.transactions.len(),
                    timestamp: block.header.timestamp,
                    gas_used: block.header.gas_used,
                    gas_limit: block.header.gas_limit,
                });
            }
        }
    }

    Ok(blocks)
}

/// Parses a JSON-RPC hex quantity with alloy's `U64`. Malformed hex returns
/// `None` instead of coercing to `0`, so a bad reply stays distinguishable
/// from a real zero-height block or a zero gas price.
///
/// An empty string (or a bare `0x` with no digits) is rejected up front:
/// `U64`'s `FromStr` treats an empty digit sequence as zero, which would turn
/// a missing quantity into a measured zero.
fn parse_hex_u64(hex: &str) -> Option<u64> {
    let digits = hex
        .strip_prefix("0x")
        .or_else(|| hex.strip_prefix("0X"))
        .unwrap_or(hex);
    if digits.is_empty() {
        return None;
    }
    hex.parse::<U64>().ok().map(|u| u.to())
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
    #[allow(clippy::result_large_err)] // stream item type is fixed by the trait bound
    fn replies(
        texts: Vec<String>,
    ) -> impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin {
        futures::stream::iter(texts.into_iter().map(|t| Ok(Message::Text(t))))
    }

    /// A structurally valid `eth_getBlockByNumber` result: full header fields
    /// alloy's `Block` requires, plus `transactions` as 32-byte hashes so
    /// `tx_count` is a real length rather than a guessed zero.
    fn block_result_json(number: u64, tx_count: usize) -> Value {
        let txs: Vec<String> = (1..=tx_count).map(|i| format!("0x{i:064x}")).collect();
        json!({
            "hash": format!("0x{:064x}", number.wrapping_add(1)),
            "parentHash": format!("0x{:064x}", number),
            "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": format!("0x{:064x}", number.wrapping_add(2)),
            "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "logsBloom": format!("0x{}", "0".repeat(512)),
            "difficulty": "0x0",
            "number": format!("0x{:x}", number),
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x5208",
            "timestamp": "0x642aa48f",
            "extraData": "0x",
            "mixHash": format!("0x{:064x}", number.wrapping_add(3)),
            "nonce": "0x0000000000000000",
            "transactions": txs,
            "uncles": []
        })
    }

    fn block_reply(id: u32, number: u64) -> String {
        // The body carries the block number that was requested; the id only
        // routes the reply. Two txs match the assertions in the
        // interleaved-noise test.
        let result = block_result_json(number, 2);
        json!({ "id": id, "result": result }).to_string()
    }

    fn null_reply(id: u32) -> String {
        format!(r#"{{"id":{},"result":null}}"#, id)
    }

    #[tokio::test]
    async fn an_error_reply_counts_as_answered_and_leaves_its_value_absent() {
        // The handshake ids are 0, 1, 2. Id 1 answers with a JSON-RPC error;
        // the stream holds exactly three replies, so completing at all proves
        // the error was counted rather than waited out.
        let mut read = replies(vec![
            r#"{"id":0,"result":"0x64"}"#.to_string(),
            r#"{"id":1,"error":{"code":-32601,"message":"eth_gasPrice is not available"}}"#
                .to_string(),
            r#"{"id":2,"result":"MockNode/0.1"}"#.to_string(),
        ]);

        let responses = collect_replies(&mut read, 3).await.unwrap();

        assert_eq!(responses.get(&0).and_then(|v| v.as_str()), Some("0x64"));
        assert!(!responses.contains_key(&1));
        assert_eq!(
            responses.get(&2).and_then(|v| v.as_str()),
            Some("MockNode/0.1")
        );
    }

    #[tokio::test]
    async fn unrelated_traffic_does_not_count_toward_the_handshake() {
        // A subscription notification and a foreign id arrive mixed in; if
        // either counted, the loop would finish before the real replies.
        let mut read = replies(vec![
            r#"{"method":"eth_subscription","params":{"result":{}}}"#.to_string(),
            r#"{"id":0,"result":"0x64"}"#.to_string(),
            r#"{"id":999,"result":"0xdeadbeef"}"#.to_string(),
            r#"{"id":1,"result":null}"#.to_string(),
            r#"{"id":2,"result":"MockNode/0.1"}"#.to_string(),
        ]);

        let responses = collect_replies(&mut read, 3).await.unwrap();

        assert_eq!(responses.len(), 2);
        assert!(!responses.contains_key(&999));
    }

    #[tokio::test]
    async fn a_stream_ending_short_of_the_handshake_is_an_error() {
        // Two replies and then the connection is gone. That is a dead
        // handshake, and the caller reconnects; pretending it completed would
        // bring the subscription up on half-initialised data.
        let mut read = replies(vec![
            r#"{"id":0,"result":"0x64"}"#.to_string(),
            r#"{"id":2,"result":"MockNode/0.1"}"#.to_string(),
        ]);

        assert!(collect_replies(&mut read, 3).await.is_err());
    }

    #[tokio::test]
    async fn a_null_result_counts_as_answered_and_the_block_is_skipped() {
        let mut write = MockSink::accepting_all();
        // Blocks 200 and 198 exist; 199 is pruned and answers null. The stream
        // holds exactly three replies, so completing at all proves nothing
        // waited on a fourth.
        let mut read = replies(vec![
            block_reply(100, 200),
            null_reply(101),
            block_reply(102, 198),
        ]);

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
        let mut read = replies(vec![block_reply(100, 200)]);

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
            block_reply(101, 199),
            r#"{"id":999,"result":"0xdeadbeef"}"#.to_string(),
            null_reply(100),
        ]);

        let blocks = fetch_blocks(&mut write, &mut read, 200, 2).await.unwrap();

        let numbers: Vec<u64> = blocks.iter().map(|b| b.number).collect();
        assert_eq!(numbers, vec![199]);
        assert_eq!(blocks[0].tx_count, 2);
    }

    #[tokio::test]
    async fn a_backfill_body_for_another_block_is_refused() {
        // Requested block 200; the node answers id 100 with a valid block-7
        // fixture. Substituting the requested number would stamp block 7's
        // hash and fields onto a row claiming to be 200.
        let mut write = MockSink::accepting_all();
        let mut read = replies(vec![block_reply(100, 7)]);

        let blocks = fetch_blocks(&mut write, &mut read, 200, 1).await.unwrap();

        assert!(
            blocks.is_empty(),
            "a body for a different block must not become the requested row: {blocks:?}"
        );
    }

    #[tokio::test]
    async fn a_backfill_body_without_a_transaction_list_is_skipped() {
        // Alloy deserializes a missing `transactions` field as
        // `BlockTransactions::Uncle` (length zero). Accepting that would
        // publish a guessed zero tx count for a body that never carried one.
        let mut body = block_result_json(200, 2);
        body.as_object_mut().unwrap().remove("transactions");

        let mut write = MockSink::accepting_all();
        let mut read = replies(vec![json!({ "id": 100, "result": body }).to_string()]);

        let blocks = fetch_blocks(&mut write, &mut read, 200, 1).await.unwrap();

        assert!(
            blocks.is_empty(),
            "listless body must be skipped: {blocks:?}"
        );
    }

    #[tokio::test]
    async fn an_explicit_empty_transaction_list_is_a_real_zero() {
        // The other side of the refusal above: `[]` is a measured zero, not a
        // missing list, and has to survive as a row with `tx_count == 0`.
        let mut write = MockSink::accepting_all();
        let mut read = replies(vec![json!({
            "id": 100,
            "result": block_result_json(200, 0),
        })
        .to_string()]);

        let blocks = fetch_blocks(&mut write, &mut read, 200, 1).await.unwrap();

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].number, 200);
        assert_eq!(blocks[0].tx_count, 0);
    }

    #[test]
    fn malformed_hex_is_not_read_as_zero() {
        assert_eq!(parse_hex_u64("0xzz"), None);
        assert_eq!(parse_hex_u64(""), None);
        assert_eq!(parse_hex_u64("0x"), None);
        // A real zero still parses; only malformed input becomes unknown.
        assert_eq!(parse_hex_u64("0x0"), Some(0));
        assert_eq!(parse_hex_u64("0x64"), Some(100));
        assert_eq!(parse_hex_u64("0x3b9aca00"), Some(1_000_000_000));
    }

    #[test]
    fn a_malformed_new_head_is_skipped_not_zeroed() {
        let mut header = block_result_json(42, 0);
        header["number"] = json!("0xzz");
        assert!(parse_new_head(&header).is_none());

        let mut missing = block_result_json(42, 0);
        missing.as_object_mut().unwrap().remove("number");
        assert!(parse_new_head(&missing).is_none());

        // A structurally valid header still becomes a Block.
        let good = parse_new_head(&block_result_json(42, 0)).expect("valid header");
        assert_eq!(good.number, 42);
        assert_eq!(good.gas_used, 0x5208);
        assert_eq!(good.gas_limit, 0x1c9c380);
    }

    #[test]
    fn colliding_suffixes_patch_the_right_rows() {
        // 100_005 and 5 share `number % 100000 == 5`. The old scheme gave
        // both the same request id and matched by suffix, so whichever reply
        // arrived second could overwrite the first row. Matching on the full
        // number carried by the id keeps them apart — including when the
        // replies arrive out of order.
        let mut data = RpcData {
            recent_blocks: vec![
                Block {
                    number: 100_005,
                    hash: "0xaa".into(),
                    tx_count: 0,
                    timestamp: 1,
                    gas_used: 1,
                    gas_limit: 1,
                },
                Block {
                    number: 5,
                    hash: "0xbb".into(),
                    tx_count: 0,
                    timestamp: 2,
                    gas_used: 2,
                    gas_limit: 2,
                },
            ],
            ..Default::default()
        };
        let mut tracker = RequestTracker::starting_at(1000);
        let id_a = tracker.next(PendingRequest::BlockByNumber(100_005));
        let id_b = tracker.next(PendingRequest::BlockByNumber(5));
        assert_ne!(id_a, id_b, "colliding suffixes must still get unique ids");

        // Out of order: the colliding suffix's smaller block answers first.
        let result_b = block_result_json(5, 3);
        let result_a = block_result_json(100_005, 7);

        let pending_b = tracker.take(id_b);
        let pending_a = tracker.take(id_a);
        assert_eq!(pending_b, Some(PendingRequest::BlockByNumber(5)));
        assert_eq!(pending_a, Some(PendingRequest::BlockByNumber(100_005)));

        // Drive `apply_block_detail` through the same number each id carried.
        if let Some(PendingRequest::BlockByNumber(n)) = pending_b {
            assert!(apply_block_detail(&mut data, n, Some(&result_b)));
        }
        if let Some(PendingRequest::BlockByNumber(n)) = pending_a {
            assert!(apply_block_detail(&mut data, n, Some(&result_a)));
        }

        assert_eq!(data.recent_blocks[0].number, 100_005);
        assert_eq!(data.recent_blocks[0].tx_count, 7);
        assert_eq!(data.recent_blocks[1].number, 5);
        assert_eq!(data.recent_blocks[1].tx_count, 3);
    }

    #[test]
    fn a_malformed_block_detail_leaves_the_row_alone() {
        let mut data = RpcData {
            recent_blocks: vec![Block {
                number: 7,
                hash: "0xcc".into(),
                tx_count: 0,
                timestamp: 1,
                gas_used: 1,
                gas_limit: 1,
            }],
            ..Default::default()
        };
        let bad = json!({ "number": "0xzz", "transactions": "not-an-array" });
        assert!(!apply_block_detail(&mut data, 7, Some(&bad)));
        assert_eq!(data.recent_blocks[0].tx_count, 0);

        // A reply for a different block number must not cross-patch either.
        let other = block_result_json(8, 5);
        assert!(!apply_block_detail(&mut data, 7, Some(&other)));
        assert_eq!(data.recent_blocks[0].tx_count, 0);
    }

    #[test]
    fn a_body_without_a_transaction_list_does_not_zero_a_nonzero_row() {
        // Alloy maps a missing `transactions` field to
        // `BlockTransactions::Uncle` (length 0). Against a row that already
        // carries a real count, accepting that would overwrite 9 with 0 —
        // the overwrite is only visible if the row starts nonzero.
        let mut data = RpcData {
            recent_blocks: vec![Block {
                number: 7,
                hash: "0xcc".into(),
                tx_count: 9,
                timestamp: 1,
                gas_used: 1,
                gas_limit: 1,
            }],
            ..Default::default()
        };

        let mut listless = block_result_json(7, 3);
        listless.as_object_mut().unwrap().remove("transactions");
        assert!(!apply_block_detail(&mut data, 7, Some(&listless)));
        assert_eq!(
            data.recent_blocks[0].tx_count, 9,
            "a body with no transaction list must not zero the row"
        );

        // Null and non-array forms are the same refusal.
        let null_txs = json!({ "number": "0x7", "transactions": null });
        assert!(!apply_block_detail(&mut data, 7, Some(&null_txs)));
        assert_eq!(data.recent_blocks[0].tx_count, 9);

        // An explicit empty array is a real zero and still applies.
        let empty = block_result_json(7, 0);
        assert!(apply_block_detail(&mut data, 7, Some(&empty)));
        assert_eq!(data.recent_blocks[0].tx_count, 0);
    }

    #[test]
    fn a_bad_gas_reply_keeps_the_previous_reading_not_zero() {
        let mut data = RpcData {
            gas_price_gwei: Some(2.0),
            ..Default::default()
        };

        assert!(!apply_gas_price(&mut data, Some(&json!("0xzz"))));
        assert_eq!(data.gas_price_gwei, Some(2.0), "malformed reply overwrote");

        assert!(!apply_gas_price(&mut data, None));
        assert_eq!(data.gas_price_gwei, Some(2.0), "absent reply erased");

        // A real zero still lands as zero — it is a measurement.
        assert!(apply_gas_price(&mut data, Some(&json!("0x0"))));
        assert_eq!(data.gas_price_gwei, Some(0.0));
    }

    /// Answers the handshake and the subscribe request, then streams `heads`
    /// `newHeads` notifications without ever answering the detail or
    /// gas-price requests those heads provoke. Block height `0x0` keeps the
    /// backfill out of the picture.
    fn heads_without_replies_server(
        heads: u32,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("read local addr");
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut ws = tokio_tungstenite::tungstenite::accept(stream).expect("upgrade");
            let mut answered = 0;
            while answered < 3 {
                if let Ok(Message::Text(text)) = ws.read() {
                    let req: Value = serde_json::from_str(&text).expect("json request");
                    let id = req["id"].as_u64().expect("request id");
                    let result = match id {
                        0 => "0x0",
                        1 => "0x3b9aca00",
                        _ => "MockNode/0.1",
                    };
                    let _ = ws.send(Message::Text(
                        json!({"id": id, "result": result}).to_string(),
                    ));
                    answered += 1;
                }
            }
            let _ = ws.read(); // subscribe request
            for n in 1..=heads {
                let header = block_result_json(n as u64, 0);
                let note = json!({
                    "jsonrpc": "2.0",
                    "method": "eth_subscription",
                    "params": { "result": header },
                });
                if ws.send(Message::Text(note.to_string())).is_err() {
                    break; // client is gone — it hit the bound and left
                }
            }
            // Hold the socket open briefly so the client fails on the bound
            // rather than on a closed stream.
            std::thread::sleep(Duration::from_millis(200));
            let _ = ws.close(None);
        });
        (addr, thread)
    }

    #[tokio::test]
    async fn continued_heads_with_missing_replies_end_the_subscription() {
        // More heads than the outstanding-request bound allows: every head
        // adds a detail and a gas-price request that never comes back, and
        // the notifications keep resetting the read timeout. The
        // subscription must give up (so the caller reconnects) instead of
        // growing the map forever.
        let heads = (MAX_PENDING_REQUESTS as u32) + 8;
        let (addr, thread) = heads_without_replies_server(heads);
        let (tx, mut rx) = mpsc::channel(8);
        // Drain as the TUI would: a full channel would park the subscription
        // on `send` before it ever reaches the outstanding-request bound.
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run_subscription_with(&format!("ws://{addr}"), &tx, Duration::from_secs(2)),
        )
        .await
        .expect("the bound must end the subscription, not hang");
        drop(tx);
        let _ = drain.await;
        let _ = thread.join();

        let err = outcome.expect_err("unanswered requests are not a healthy stream");
        let text = format!("{:#}", err);
        assert!(
            text.contains("unanswered"),
            "should name the unanswered requests: {text}"
        );
    }

    #[tokio::test]
    async fn unanswered_requests_that_stay_under_the_bound_do_not_end_the_stream() {
        // The other side of the bound: a short gap with no replies is not
        // itself fatal, so a healthy (if momentarily slow) node is not
        // torn down by the same check.
        assert!(RequestTracker::starting_at(1000).will_fit(2));

        let mut tracker = RequestTracker::starting_at(1000);
        for i in 0..MAX_PENDING_REQUESTS {
            let id = tracker.next(PendingRequest::BlockByNumber(i as u64));
            assert!(id >= 1000);
        }
        assert_eq!(tracker.pending_len(), MAX_PENDING_REQUESTS);
        assert!(
            !tracker.will_fit(2),
            "the bound must refuse further pairs once full"
        );
        // Taking a reply frees room again — the normal path.
        tracker.take(1000);
        assert!(tracker.will_fit(1));
    }

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// An endpoint whose kernel accepts the socket and whose process never
    /// answers the WebSocket upgrade. Accepted streams are parked, not
    /// dropped: a closed socket would fail the connect at once, which is not
    /// the hang this guards against. Dropping the guard stops the thread.
    struct HeldListener {
        addr: std::net::SocketAddr,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl HeldListener {
        fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let addr = listener.local_addr().expect("read local addr");
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                let mut held = Vec::new();
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        held.push(stream);
                    }
                }
            });
            Self {
                addr,
                stop,
                thread: Some(thread),
            }
        }

        fn url(&self) -> String {
            format!("ws://{}", self.addr)
        }
    }

    impl Drop for HeldListener {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake the accept loop so it sees the flag, then wait for it.
            let _ = std::net::TcpStream::connect(self.addr);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// A server that answers the upgrade and the opening requests, stays
    /// quiet for `quiet` before closing, then drains until the client is
    /// gone. Block number 0 keeps the backfill out of the picture.
    fn healthy_server(quiet: Duration) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("read local addr");
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut ws = tokio_tungstenite::tungstenite::accept(stream).expect("upgrade");
            let mut answered = 0;
            while answered < 3 {
                if let Message::Text(text) = ws.read().expect("read an opening request") {
                    let req: Value = serde_json::from_str(&text).expect("json request");
                    let id = req["id"].as_u64().expect("request id");
                    let result = match id {
                        0 => "0x0",
                        1 => "0x3b9aca00",
                        _ => "MockNode/0.1",
                    };
                    ws.send(Message::Text(
                        json!({"id": id, "result": result}).to_string(),
                    ))
                    .expect("send reply");
                    answered += 1;
                }
            }
            let _ = ws.read().expect("read the subscribe request");
            std::thread::sleep(quiet);
            let _ = ws.close(None);
            while ws.read().is_ok() {}
        });
        (addr, thread)
    }

    #[tokio::test]
    async fn a_connect_that_never_completes_fails_by_its_deadline() {
        // Without a deadline on the connect this test does not fail, it hangs:
        // the outer timeout is what turns a regression into a red test.
        let server = HeldListener::start();
        let (tx, mut rx) = mpsc::channel(8);

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run_subscription_with(&server.url(), &tx, Duration::from_millis(250)),
        )
        .await
        .expect("the connect must fail by its own deadline, not hang");

        let err = outcome.expect_err("an upgrade that never completes is not a connection");
        let text = format!("{:#}", err);
        assert!(
            text.contains("timed out"),
            "should read as a timeout: {text}"
        );
        assert!(
            text.contains(&server.addr.to_string()),
            "should name the endpoint: {text}"
        );
        // The caller turns the error into `Disconnected`; the function itself
        // sends nothing before the stream is up.
        assert!(rx.try_recv().is_err());

        // A later connect to an endpoint that does answer still works.
        let (addr, thread) = healthy_server(Duration::from_millis(50));
        let later = tokio::time::timeout(
            Duration::from_secs(5),
            run_subscription_with(&format!("ws://{addr}"), &tx, Duration::from_millis(250)),
        )
        .await
        .expect("a healthy endpoint must not hang either");
        let _ = thread.join();
        assert!(
            matches!(later, Ok(true)),
            "the later connect should stream: {later:?}"
        );
    }

    #[tokio::test]
    async fn a_completed_upgrade_is_not_cut_off_by_the_connect_deadline() {
        // The server answers the upgrade and the opening requests, then stays
        // quiet for longer than the connect deadline before closing: the
        // deadline must stop counting once the upgrade is done.
        let (addr, thread) = healthy_server(Duration::from_millis(600));
        let (tx, mut rx) = mpsc::channel(8);

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run_subscription_with(&format!("ws://{addr}"), &tx, Duration::from_millis(250)),
        )
        .await
        .expect("a server that closes after the upgrade must not hang the client");
        let _ = thread.join();

        assert!(
            matches!(outcome, Ok(true)),
            "the connection should have streamed: {outcome:?}"
        );
        assert!(matches!(rx.try_recv(), Ok(RpcEvent::Data(_))));
        assert!(matches!(rx.try_recv(), Ok(RpcEvent::Connected)));
    }
}
