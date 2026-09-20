//! `/api/*` request handling.

use std::sync::Arc;

use axum::extract::{Path, State};
use explorer_core::fmt::{remove_bad_chars, timestamp_utc};
use explorer_core::{BlockId, BlockIdError, ChainError, Hash32, RpcChainSource, unexpanded_inputs};
use monerod_rpc::types::{BlockHeader, GetTxidsLooseRequest, TxEntry};
use serde::Serialize;

use super::envelope::{ApiError, ApiOk};
use super::shapes::{BlockDetail, TxDetail, TxSummary, normalise_hash};

pub struct AppState {
    pub chain: RpcChainSource,
    /// Whether the daemon answered the `get_txids_loose` probe at startup.
    ///
    /// Probed once rather than per request. The documentation page reports it,
    /// and rendering a page of prose should not cost a round trip to answer a
    /// question whose answer cannot change while the daemon stays up. Set by
    /// `main` after the probe it already makes for the startup log.
    pub txids_loose: std::sync::atomic::AtomicBool,
}

pub type Shared = State<Arc<AppState>>;

/// Map a chain failure onto upstream's two outcomes.
///
/// `fail` means the caller asked for something that is not there; `error`
/// means we could not answer. Upstream draws the line the same way, and the
/// distinction is the only signal a client gets, since both are HTTP 200.
fn on_chain_error(e: &ChainError, what: &str) -> ApiError {
    if e.is_not_found() {
        ApiError::fail(what.to_owned())
    } else {
        // The operator gets the detail; the client does not. See
        // ChainError::public_message.
        tracing::warn!("{what}: {e}");
        ApiError::error(e.public_message())
    }
}

// ---------------------------------------------------------------------------
// /api/version
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct VersionData {
    /// `(major << 16) | minor`. We implement the 1.3 shapes, so 65539.
    api: u64,
    blockchain_height: u64,
    /// Empty: oxblocks does not embed build metadata. The keys exist because
    /// clients index them; inventing values would be worse than admitting we
    /// have none.
    git_branch_name: String,
    last_git_commit_date: String,
    last_git_commit_hash: String,
    monero_version_full: String,
    /// Not an upstream key. oxblocks is not the C++ explorer, and a client
    /// that wants to know which implementation it is talking to should not
    /// have to guess from the absence of a commit hash.
    oxblocks_version: String,
}

pub async fn version(State(state): Shared) -> Result<ApiOk<VersionData>, ApiError> {
    let info = state
        .chain
        .info()
        .await
        .map_err(|e| on_chain_error(&e, "Cant get daemon info"))?;

    Ok(ApiOk(VersionData {
        api: (1 << 16) | 3,
        blockchain_height: info.height,
        git_branch_name: String::new(),
        last_git_commit_date: String::new(),
        last_git_commit_hash: String::new(),
        monero_version_full: info.version.clone(),
        oxblocks_version: env!("CARGO_PKG_VERSION").to_owned(),
    }))
}

// ---------------------------------------------------------------------------
// /api/transaction/<hash>
// ---------------------------------------------------------------------------

pub async fn transaction(
    State(state): Shared,
    Path(raw): Path<String>,
) -> Result<ApiOk<TxDetail>, ApiError> {
    let cleaned = remove_bad_chars(&raw);
    let hash: Hash32 = cleaned
        .parse()
        // Upstream echoes the *sanitised* argument, not the raw one.
        .map_err(|_| ApiError::fail(format!("Cant parse tx hash: {cleaned}")))?;

    let fetched = state
        .chain
        .transactions(std::slice::from_ref(&hash))
        .await
        .map_err(|e| on_chain_error(&e, &format!("Cant get tx: {hash}")))?;

    let Some(entry) = fetched.txs.first() else {
        return Err(ApiError::fail(format!("Cant find tx: {hash}")));
    };

    let tx = entry
        .parse_json()
        .map_err(|e| ApiError::error(format!("Cant parse tx {hash}: {e}")))?;

    // One /get_outs per input. Never batched across the transaction: monerod
    // fails the whole request if any single index is out of range, which would
    // let one bad input blank every ring on the page.
    let rings = state.chain.resolve_rings(&tx).await;

    // A confirmed transaction already carries its confirmation count, so the
    // tip can be derived without a second round trip. A pool transaction has
    // neither, and needs one.
    let current_height = if entry.in_pool {
        state
            .chain
            .info()
            .await
            .map_err(|e| on_chain_error(&e, "Cant get daemon info"))?
            .height
    } else {
        entry.block_height.saturating_add(entry.confirmations)
    };

    Ok(ApiOk(TxDetail::build(entry, &tx, &rings, current_height)))
}

// ---------------------------------------------------------------------------
// /api/block/<height|hash>
// ---------------------------------------------------------------------------

/// Wrap [`BlockId::parse`] in upstream's wording, which differs by which shape
/// was attempted.
fn parse_block_id(cleaned: &str) -> Result<BlockId, ApiError> {
    BlockId::parse(cleaned).map_err(|e| match e {
        BlockIdError::NotAHash => ApiError::fail(format!("Cant parse blk hash: {cleaned}")),
        BlockIdError::NotAHeight | BlockIdError::Unrecognised => {
            ApiError::fail(format!("Cant find blk using search string: {cleaned}"))
        }
    })
}

/// How upstream words a missing block, which differs by how it was asked for.
///
/// For a hash the message carries **literal angle brackets** around a
/// lowercased hash, because fmt routes `crypto::hash` through monero's
/// `operator<<`, which writes `<` and `>`. Confirmed against a live upstream
/// deployment. The spec wrote `<hash>` as a metavariable and hid it.
fn block_not_found(id: BlockId) -> String {
    match id {
        BlockId::Height(h) => format!("Cant get block: {h}"),
        BlockId::Hash(h) => format!("Cant get block: <{h}>"),
    }
}

pub async fn block(
    State(state): Shared,
    Path(raw): Path<String>,
) -> Result<ApiOk<BlockDetail>, ApiError> {
    let cleaned = remove_bad_chars(&raw);
    let id = parse_block_id(&cleaned)?;
    Ok(ApiOk(build_block_detail(&state, id).await?))
}

/// Assemble one block's API representation.
///
/// Shared by `/api/block` and `/api/blocks/<start>/<end>`, because the element
/// type of the range response is exactly the single-block response.
async fn build_block_detail(state: &AppState, id: BlockId) -> Result<BlockDetail, ApiError> {
    let got = state
        .chain
        .block(id)
        .await
        .map_err(|e| on_chain_error(&e, &block_not_found(id)))?;

    // The miner transaction plus every other transaction in the block, in the
    // order the block lists them, coinbase first.
    let mut hashes: Vec<Hash32> = Vec::with_capacity(got.tx_hashes.len() + 1);
    hashes.extend(got.miner_tx_hash.parse::<Hash32>());
    hashes.extend(
        got.tx_hashes
            .iter()
            .filter_map(|h| h.parse::<Hash32>().ok()),
    );

    let fetched = state
        .chain
        .transactions(&hashes)
        .await
        .map_err(|e| on_chain_error(&e, &block_not_found(id)))?;

    Ok(block_detail(&got.block_header, &fetched.txs))
}

/// One block's API representation, from its header and its transactions.
///
/// Shared by `/api/block` and `/api/blocks/<start>/<end>`, because the element
/// type of the range response is exactly the single-block response. Takes the
/// header rather than a whole `get_block`, because a range is answered from
/// `get_block_headers_range` and never fetches the block body at all unless
/// the block holds something.
fn block_detail(header: &BlockHeader, entries: &[TxEntry]) -> BlockDetail {
    let mut txs = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.parse_json() {
            Ok(tx) => txs.push(TxSummary::build(entry, &tx)),
            // One undecodable transaction must not lose the whole block page.
            Err(e) => tracing::warn!(tx = %entry.tx_hash, "skipping: {e}"),
        }
    }

    BlockDetail {
        block_height: header.height,
        // The tip, derived from this block's own depth rather than a second
        // round trip: `depth` is 0 for the tip, and `current_height` is the
        // chain *height* (tip + 1). Checked against a real upstream capture --
        // block 2,000,000 at depth 1,765,612 reports current_height 3,765,613.
        current_height: header.height.saturating_add(header.depth).saturating_add(1),
        hash: normalise_hash(&header.hash),
        size: header.block_size,
        timestamp: header.timestamp,
        timestamp_utc: timestamp_utc(header.timestamp),
        txs,
    }
}

// ---------------------------------------------------------------------------
// /api/rawblock/<height|hash> and /api/rawtransaction/<hash>
// ---------------------------------------------------------------------------

/// The block exactly as monerod decoded it.
///
/// `data` is `get_block`'s nested `json` string, reparsed and re-serialised.
/// The reparse is not ceremony: monerod emits that document in *declaration*
/// order (`major_version, minor_version, timestamp, prev_id, nonce, miner_tx,
/// tx_hashes`) and upstream re-sorts it alphabetically on the way out, so
/// forwarding the string verbatim would be byte-wrong.
pub async fn raw_block(
    State(state): Shared,
    Path(raw): Path<String>,
) -> Result<ApiOk<serde_json::Value>, ApiError> {
    let cleaned = remove_bad_chars(&raw);
    let id = parse_block_id(&cleaned)?;

    let got = state
        .chain
        .block(id)
        .await
        .map_err(|e| on_chain_error(&e, &block_not_found(id)))?;

    let value: serde_json::Value = serde_json::from_str(&got.json)
        .map_err(|_| ApiError::error("Faild parsing raw blk data into json".to_owned()))?;
    Ok(ApiOk(value))
}

pub async fn raw_transaction(
    State(state): Shared,
    Path(raw): Path<String>,
) -> Result<ApiOk<serde_json::Value>, ApiError> {
    let cleaned = remove_bad_chars(&raw);
    let hash: Hash32 = cleaned
        .parse()
        .map_err(|_| ApiError::fail(format!("Cant parse tx hash: {cleaned}")))?;

    let fetched = state
        .chain
        .transactions(std::slice::from_ref(&hash))
        .await
        .map_err(|e| on_chain_error(&e, &format!("Cant get tx: {hash}")))?;

    let Some(entry) = fetched.txs.first() else {
        return Err(ApiError::fail(format!("Cant find tx: {hash}")));
    };

    let value: serde_json::Value = serde_json::from_str(&entry.as_json)
        .map_err(|_| ApiError::error("Faild parsing raw tx data into json".to_owned()))?;
    Ok(ApiOk(value))
}

// ---------------------------------------------------------------------------
// /api/feeestimate
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct FeeData {
    fee: u64,
    /// Upstream reports the same number twice: the per-byte estimate is what
    /// monerod returns, and `fee_per_kb` kept its name from before the
    /// per-byte switch. Reproduced rather than corrected.
    fee_per_kb: u64,
    grace_blocks: u64,
}

#[derive(serde::Deserialize)]
pub struct GraceQuery {
    grace_blocks: Option<String>,
}

pub async fn fee_estimate(
    State(state): Shared,
    axum::extract::Query(q): axum::extract::Query<GraceQuery>,
) -> Result<ApiOk<FeeData>, ApiError> {
    // Upstream only honours the parameter when it is all digits, and otherwise
    // silently uses the default rather than erroring.
    let grace_blocks = q
        .grace_blocks
        .as_deref()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10);

    let estimate = state
        .chain
        .fee_estimate(grace_blocks)
        .await
        .map_err(|_| ApiError::error("Cant get dynamic fee estimate".to_owned()))?;

    Ok(ApiOk(FeeData {
        fee: estimate.fee,
        fee_per_kb: estimate.fee,
        grace_blocks,
    }))
}

// ---------------------------------------------------------------------------
// /api/transactions and /api/mempool
// ---------------------------------------------------------------------------

/// The largest `limit` `/api/transactions` will honour.
///
/// **A deliberate divergence from upstream, for safety rather than taste.**
///
/// Upstream does not clamp this. It can afford not to: it reads its own local
/// database, so a large page costs it local disk reads. Every block here is a
/// network round trip to the daemon -- one `get_block` for each block that
/// holds transactions, because headers carry no `tx_hashes`, plus one
/// `get_transactions` for the page.
///
/// Before this cap existed, a single unauthenticated `?limit=200` request
/// took 19 seconds of daemon work and returned 4 MB, and the header range cap
/// of 1000 meant one request could reach a thousand blocks -- against a
/// daemon that would accept 128 such requests at once, with no bound on how
/// many calls were in flight. That is a denial-of-service amplifier reachable
/// by anyone who can make an HTTP request.
///
/// The figure first written here, "10,190 RPC calls", was wrong: it counted
/// cache *misses*, and one `/get_transactions` carrying forty hashes is forty
/// misses and one call. The true count was about four hundred. That mistake is
/// why [`explorer_core::RpcChainSource::rpc_calls`] exists -- the number that
/// matters is now counted rather than inferred. At the cap today a page costs
/// one header range, one `get_block` per block that holds transactions, and
/// one batched `get_transactions`: 52 calls on a full chain, and 3 on a quiet
/// one, measured.
///
/// Clamped rather than rejected, because upstream silently substitutes a
/// default for input it will not use, and a clamp keeps that habit.
pub const MAX_TRANSACTIONS_LIMIT: u64 = 50;

/// The largest `limit` `/api/mempool` will honour.
///
/// Bounded by the pool's own size in practice, so this is a ceiling on
/// response size rather than on daemon work.
pub const MAX_MEMPOOL_LIMIT: u64 = 500;

#[derive(serde::Deserialize)]
pub struct PageQuery {
    page: Option<String>,
    limit: Option<String>,
}

impl PageQuery {
    /// Upstream reads a query parameter only when its raw text matches
    /// `\d+`, and otherwise silently substitutes the default. An
    /// unparseable value is not an error there, so it is not one here.
    fn parse(&self, default_limit: u64, max_limit: u64) -> (u64, u64) {
        fn digits(v: Option<&String>) -> Option<u64> {
            let s = v?;
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse().ok())
                .flatten()
        }
        (
            digits(self.page.as_ref()).unwrap_or(0),
            digits(self.limit.as_ref())
                .unwrap_or(default_limit)
                .min(max_limit),
        )
    }
}

#[derive(Serialize)]
pub struct BlockRow {
    age: String,
    hash: String,
    height: u64,
    /// A JSON **float** here, and an integer in `/api/block`. Upstream holds
    /// the same value in a `double` in one builder and a `uint64_t` in the
    /// other; on one block that is 95511.0 against 95511.
    size: f64,
    timestamp: u64,
    timestamp_utc: String,
    txs: Vec<TxSummary>,
}

#[derive(Serialize)]
pub struct TransactionsData {
    blocks: Vec<BlockRow>,
    current_height: u64,
    limit: u64,
    page: u64,
    total_page_no: u64,
}

pub async fn transactions(
    State(state): Shared,
    axum::extract::Query(q): axum::extract::Query<PageQuery>,
) -> Result<ApiOk<TransactionsData>, ApiError> {
    let (page, limit) = q.parse(25, MAX_TRANSACTIONS_LIMIT);

    let info = state
        .chain
        .info()
        .await
        .map_err(|e| on_chain_error(&e, "Cant get daemon info"))?;
    let height = info.height;

    // Upstream computes the window in UNSIGNED arithmetic and only then
    // narrows to int64, so a large `page` wraps modulo 2^64 and lands on
    // recent blocks rather than erroring. Reproduced with explicit wrapping:
    // a faithful transcription would panic here, because this build keeps
    // overflow-checks on in release.
    let span = limit.wrapping_mul(page.wrapping_add(1));
    #[allow(
        clippy::cast_possible_wrap,
        reason = "the wrap is the behaviour being reproduced; see above"
    )]
    let start_signed = (height.wrapping_sub(span)) as i64;
    #[allow(clippy::cast_sign_loss, reason = "max(0) has already removed the sign")]
    let start = start_signed.max(0) as u64;
    let end = start.saturating_add(limit).min(height).saturating_sub(1);

    let mut blocks = Vec::new();
    if start < height && limit > 0 {
        // One fan-out for the whole page rather than two calls per block. The
        // partially-built array still travels with the error, because upstream
        // assigns `data["blocks"]` before the loop that can fail -- it is
        // empty here, since a batch either arrives or does not.
        let fetched = state.chain.blocks_in_range(start, end).await.map_err(|e| {
            let partial = serde_json::json!({ "blocks": [] });
            if e.is_not_found() {
                ApiError::fail(format!("Cant get block: {start}"))
            } else {
                ApiError::error(format!("Cant get transactions in block: {start}"))
            }
            .with_partial(partial)
        })?;

        // Newest first, which is the order a page of recent blocks is read in.
        for block in fetched.iter().rev() {
            let header = &block.header;
            blocks.push(BlockRow {
                age: explorer_core::fmt::age(explorer_core::fmt::now(), header.timestamp),
                hash: normalise_hash(&header.hash),
                height: header.height,
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "upstream holds this in a double here and an                               integer in /api/block, and the float is the                               observable difference we reproduce; block                               weights are far below 2^53 in any case"
                )]
                size: header.block_size as f64,
                timestamp: header.timestamp,
                timestamp_utc: timestamp_utc(header.timestamp),
                txs: block
                    .txs
                    .iter()
                    .filter_map(|e| e.parse_json().ok().map(|tx| TxSummary::build(e, &tx)))
                    .collect(),
            });
        }
    }

    Ok(ApiOk(TransactionsData {
        blocks,
        current_height: height,
        limit,
        page,
        // Ceiling division, and a deliberate divergence: upstream writes
        // `height / limit`, which floors, so the final partial page is not
        // counted. A client that trusts the number stops one page early and
        // never sees the oldest blocks -- at height 137,082 with 25 per page
        // it reports 5,483 pages when page 5,483 exists and holds seven
        // blocks. The same floor is in upstream's mempool count and its HTML
        // index. Guards a zero limit, which upstream also special-cases.
        total_page_no: if limit == 0 {
            0
        } else {
            height.div_ceil(limit)
        },
    }))
}

#[derive(Serialize)]
pub struct MempoolData {
    limit: u64,
    page: u64,
    total_page_no: u64,
    txs: Vec<serde_json::Value>,
    txs_no: u64,
}

pub async fn mempool(
    State(state): Shared,
    axum::extract::Query(q): axum::extract::Query<PageQuery>,
) -> Result<ApiOk<MempoolData>, ApiError> {
    // Upstream's default is effectively unbounded; it only pages when asked.
    // Ours is capped, for the reason on MAX_MEMPOOL_LIMIT.
    let (page, limit) = q.parse(MAX_MEMPOOL_LIMIT, MAX_MEMPOOL_LIMIT);

    let pool = state.chain.mempool().await.map_err(|e| match e {
        ChainError::NeedsUnrestricted(what) => ApiError::error(format!(
            "{what} needs an unrestricted daemon; this one blocks /get_transaction_pool"
        )),
        other => {
            tracing::warn!("mempool: {other}");
            ApiError::error(other.public_message())
        }
    })?;

    let txs_no = pool.transactions.len() as u64;
    let skip = page.saturating_mul(limit);

    let mut txs = Vec::new();
    for info in pool
        .transactions
        .iter()
        // try_from rather than `as`: on a 32-bit target a u64 beyond
        // usize::MAX would truncate to a small number and silently skip the
        // wrong rows. Saturating is the honest reading of "past the end".
        .skip(usize::try_from(skip).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
    {
        let Ok(tx) = info.parse_json() else { continue };
        // A pool transaction has no block, so its size is the pool entry's own
        // blob size rather than a reassembled hex blob.
        let mut value = match serde_json::to_value(TxSummary::build_pool(info, &tx)) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(map) = value.as_object_mut() {
            map.insert("timestamp".to_owned(), serde_json::json!(info.receive_time));
            map.insert(
                "timestamp_utc".to_owned(),
                serde_json::json!(timestamp_utc(info.receive_time)),
            );
        }
        txs.push(value);
    }

    Ok(ApiOk(MempoolData {
        limit,
        page,
        total_page_no: if limit == 0 {
            0
        } else {
            txs_no.div_ceil(limit)
        },
        txs,
        txs_no,
    }))
}

// ---------------------------------------------------------------------------
// /api/search/<height|hash>
// ---------------------------------------------------------------------------

/// Search dispatches on the *shape* of the sanitised argument: a short numeric
/// string is a height, a 64-character hex string is tried as a block hash and
/// then as a transaction hash.
///
/// The result is the matching block or transaction object with a `title`
/// naming which it is, so a client can tell them apart without re-inspecting
/// the fields.
///
/// Monero has no address index, so an address is not searchable here and
/// never can be without one. Saying so is more useful than a bare failure.
pub async fn search(
    State(state): Shared,
    Path(raw): Path<String>,
) -> Result<ApiOk<serde_json::Value>, ApiError> {
    let cleaned = remove_bad_chars(&raw);

    if let Ok(BlockId::Height(height)) = BlockId::parse(&cleaned) {
        let block = block(State(state), Path(height.to_string())).await?;
        return Ok(ApiOk(titled(block.0, "block")));
    }

    if cleaned.len() == 64 {
        // A block hash and a transaction hash are the same shape, so the only
        // way to tell them apart is to try one and then the other.
        if let Ok(found) = block(State(Arc::clone(&state)), Path(cleaned.clone())).await {
            return Ok(ApiOk(titled(found.0, "block")));
        }
        if let Ok(found) = transaction(State(state), Path(cleaned.clone())).await {
            return Ok(ApiOk(titled(found.0, "tx")));
        }
        return Err(ApiError::fail(format!(
            "Cant find blk or tx using search string: {cleaned}"
        )));
    }

    if cleaned.len() > 64 {
        return Err(ApiError::fail(format!(
            "Cant find blk or tx using search string: {cleaned}. Monero has no \
             address index, so addresses are not searchable"
        )));
    }

    Err(ApiError::fail(format!(
        "Cant find blk or tx using search string: {cleaned}"
    )))
}

fn titled<T: Serialize>(payload: T, title: &str) -> serde_json::Value {
    let mut value = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    if let Some(map) = value.as_object_mut() {
        map.insert("title".to_owned(), serde_json::json!(title));
    }
    value
}

// ---------------------------------------------------------------------------
// /api/networkinfo
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct NetworkInfoData {
    alt_blocks_count: u64,
    block_size_limit: u64,
    block_size_median: u64,
    /// A JSON **string**, not a number. Upstream renders 128-bit difficulty
    /// through a decimal-string helper because it does not fit a double, and
    /// the README's numeric example is stale.
    cumulative_difficulty: String,
    current: bool,
    current_hf_version: u8,
    difficulty: String,
    fee_estimate: u64,
    fee_estimate_grace_blocks: u64,
    fee_per_kb: u64,
    grey_peerlist_size: u64,
    hash_rate: u64,
    height: u64,
    incoming_connections_count: u64,
    outgoing_connections_count: u64,
    stagenet: bool,
    start_time: u64,
    /// A boolean field of `data`, distinct from the envelope's `"status":
    /// "success"` string. Do not conflate them.
    status: bool,
    target: u64,
    target_height: u64,
    testnet: bool,
    top_block_hash: String,
    tx_count: u64,
    tx_pool_size: u64,
    tx_pool_size_kbytes: u64,
    white_peerlist_size: u64,
}

pub async fn network_info(State(state): Shared) -> Result<ApiOk<NetworkInfoData>, ApiError> {
    let info = state
        .chain
        .info()
        .await
        .map_err(|e| on_chain_error(&e, "Cant get daemon info"))?;

    let fee = state.chain.fee_estimate(10).await.ok();

    // The tip block's major_version *is* the active hard-fork version, so this
    // needs no separate hard_fork_info call.
    let hf = state
        .chain
        .last_block_header()
        .await
        .ok()
        .map(|h| h.block_header.major_version);

    // Despite the name, this field carries **bytes**. Upstream sums the pool's
    // `blob_size` into `MempoolStatus::mempool_size`, whose own declaration
    // says "size in bytes", and publishes it under the kbytes name unchanged --
    // on master and on devel alike. Dividing by 1024 to honour the name would
    // hand every existing consumer a number 1024 times too small.
    //
    // Taken from `get_transaction_pool_stats`, which is the same total without
    // the pool attached to it. Unavailable on a restricted daemon, in which
    // case report zero rather than failing the whole page.
    let pool_bytes = state
        .chain
        .pool_stats()
        .await
        .ok()
        .map(|p| p.pool_stats.bytes_total);

    Ok(ApiOk(NetworkInfoData {
        alt_blocks_count: info.alt_blocks_count,
        block_size_limit: info.block_size_limit,
        block_size_median: info.block_size_median,
        cumulative_difficulty: info.cumulative_difficulty().to_string(),
        current: true,
        current_hf_version: hf.unwrap_or(0),
        difficulty: info.difficulty().to_string(),
        // master reports the real estimate here and states the grace window
        // beside it; devel replaced the first with a variable it never assigns
        // -- a constant zero -- and dropped the second. Following master: a
        // dead field is not a contract worth reproducing.
        fee_estimate: fee.as_ref().map_or(0, |f| f.fee),
        fee_estimate_grace_blocks: 10,
        fee_per_kb: fee.as_ref().map_or(0, |f| f.fee),
        grey_peerlist_size: info.grey_peerlist_size,
        // Network hash rate is difficulty spread over the target block time.
        hash_rate: if info.target == 0 {
            0
        } else {
            // Reassembled difficulty, not the low word: correct today because
            // mainnet's top64 is still zero, and correct later when it is not.
            u64::try_from(info.difficulty() / u128::from(info.target)).unwrap_or(u64::MAX)
        },
        height: info.height,
        incoming_connections_count: info.incoming_connections_count,
        outgoing_connections_count: info.outgoing_connections_count,
        stagenet: info.stagenet,
        start_time: info.start_time,
        status: true,
        target: info.target,
        target_height: info.target_height,
        testnet: info.testnet,
        top_block_hash: normalise_hash(&info.top_block_hash),
        tx_count: info.tx_count,
        tx_pool_size: info.tx_pool_size,
        tx_pool_size_kbytes: pool_bytes.unwrap_or(0),
        white_peerlist_size: info.white_peerlist_size,
    }))
}

// ---------------------------------------------------------------------------
// /api/transaction/private/<postfix>   — k-anonymous transaction lookup
// ---------------------------------------------------------------------------

/// Shortest postfix accepted. Below this the response is enormous.
pub const MIN_POSTFIX_LEN: usize = 2;

/// Longest postfix accepted, regardless of chain size.
pub const MAX_POSTFIX_LEN: usize = 12;

/// The fewest transactions a postfix must be *expected* to match before the
/// lookup is worth serving.
///
/// Each further hex character divides the expected set by sixteen, so this is
/// what decides how long a postfix a given chain will accept. It has to be
/// well clear of one rather than merely above it: how many transactions
/// actually share a postfix is Poisson around the expected number, so at an
/// expected 2 a request has roughly a 40% chance of returning one transaction
/// or none — which is no anonymity at all. At 20 that is about 4 in 100
/// million.
///
/// On mainnet this permits 5 characters and refuses 6, so the smallest set
/// served is around 40 transactions. Matches upstream devel exactly.
pub const MIN_ANONYMITY_SET: u64 = 20;

/// The most matches this explorer will expand before refusing.
///
/// Checked twice, against two different numbers. [`check_postfix`] refuses a
/// postfix whose *expected* set is already larger than this, before the daemon
/// is asked anything; the handler refuses again on the count that actually came
/// back, because how many transactions share a postfix is Poisson around the
/// expectation rather than equal to it.
///
/// The first check is the one that matters for load. `get_txids_loose` walks
/// the whole transaction index, so a two-character postfix on mainnet is a
/// full-index scan answering with something like a quarter of a million hashes
/// -- ten megabytes off the daemon -- only for this explorer to then refuse to
/// serve them. Refusing on the arithmetic costs nothing and says the same
/// thing.
pub const MAX_PRIVATE_TX_MATCHES: u64 = 1000;

/// The two bounds have to leave a usable band between them on every chain
/// size, or the endpoint would refuse everything while reporting two different
/// reasons for it.
///
/// One extra character divides the expected set by sixteen, so consecutive
/// postfix lengths are a factor of sixteen apart: unless the band is at least
/// that wide, a chain can fall between two adjacent lengths and accept no
/// postfix at all. Asserted at compile time rather than in a test, because a
/// runtime check on two constants can only ever pass.
const _: () = assert!(
    MAX_PRIVATE_TX_MATCHES / MIN_ANONYMITY_SET >= 16,
    "the anonymity floor and the serving ceiling are less than one postfix \
     character apart, so some chain sizes would accept no postfix at all"
);

/// Every transaction on the chain, coinbase included.
///
/// `get_info.tx_count` counts only **non-coinbase** transactions, which on a
/// quiet chain is a tiny fraction of the total: the local testnet reports 14
/// against a real 134,875. Upstream reads `get_db().get_tx_count()`, which
/// includes them, and the anonymity rule divides by this number — so using the
/// RPC field directly refuses postfixes that are perfectly anonymous.
///
/// Each block carries exactly one coinbase transaction, so the total is the
/// non-coinbase count plus the height. Checked against upstream on the same
/// chain: 14 + 134,861 = 134,875, which is what it reports.
fn total_transactions(info: &monerod_rpc::types::GetInfo) -> u64 {
    info.tx_count.saturating_add(info.height)
}

/// Why a postfix cannot be served.
enum PostfixRefusal {
    Length,
    NotHex,
    TooLongToBeAnonymous { tx_count: u64 },
    TooShortToServe { expected: u64 },
}

/// Validate a postfix against the anonymity rule and against what this
/// explorer will serve.
///
/// Separated from the handler so both rules can be tested without a daemon:
/// the first is the whole privacy property, and "it looked right" is not
/// evidence.
///
/// The accepted band is expressed entirely in expected matches — at least
/// [`MIN_ANONYMITY_SET`], at most [`MAX_PRIVATE_TX_MATCHES`] — so it scales
/// with the chain rather than hard-coding a length. On mainnet today that
/// admits five characters and no others; on the local testnet it is two or
/// three. Which lengths qualify moves as the chain grows, which is the point
/// of stating the rule in matches.
fn check_postfix(postfix: &str, tx_count: u64) -> Result<(), PostfixRefusal> {
    if postfix.len() < MIN_POSTFIX_LEN || postfix.len() > MAX_POSTFIX_LEN {
        return Err(PostfixRefusal::Length);
    }
    if !postfix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(PostfixRefusal::NotHex);
    }
    // Each hex character is four bits, so each one divides the expected set by
    // sixteen.
    let shift = u32::try_from(postfix.len() * 4).unwrap_or(u32::MAX);
    let expected = tx_count.checked_shr(shift).unwrap_or(0);
    // Too long: the chain is not expected to hold `MIN_ANONYMITY_SET` matches,
    // so the postfix identifies its transaction rather than hiding it, which is
    // the opposite of the point.
    if expected < MIN_ANONYMITY_SET {
        return Err(PostfixRefusal::TooLongToBeAnonymous { tx_count });
    }
    // Too short: the set is bigger than this explorer will expand, so the
    // request ends in a refusal either way. Reaching that conclusion here
    // rather than after the daemon has scanned its whole transaction index is
    // the difference between a rejected request and an amplified one.
    if expected > MAX_PRIVATE_TX_MATCHES {
        return Err(PostfixRefusal::TooShortToServe { expected });
    }
    Ok(())
}

/// Which postfix lengths this chain currently accepts.
///
/// Asks [`check_postfix`] rather than repeating its arithmetic, so the
/// documentation page cannot describe a rule the endpoint does not enforce.
/// Which lengths qualify moves as the chain grows, which is why the rule is
/// stated in expected matches and this is computed rather than written down.
#[must_use]
pub fn acceptable_postfix_lengths(info: &monerod_rpc::types::GetInfo) -> Vec<usize> {
    let total = total_transactions(info);
    (MIN_POSTFIX_LEN..=MAX_POSTFIX_LEN)
        .filter(|n| check_postfix(&"a".repeat(*n), total).is_ok())
        .collect()
}

/// Narrow a hex postfix to a whole number of bytes, keeping the **trailing**
/// characters.
///
/// monerod matches on bits but parses a hash template, so only whole bytes can
/// be expressed; an odd-length postfix has to search one character narrower and
/// filter afterwards. Because it is a suffix, the character dropped is the
/// leading one: `"abc"` searches `"bc"`. Taking the front instead returns
/// nothing at all, and silently — the request succeeds with an empty set.
fn whole_byte_suffix(postfix: &str) -> &str {
    let keep = postfix.len() - postfix.len() % 2;
    postfix.get(postfix.len() - keep..).unwrap_or_default()
}

#[derive(Serialize)]
pub struct PrivateTxData {
    missed_txs: Vec<String>,
    txs: Vec<TxDetail>,
}

pub async fn transaction_private(
    State(state): Shared,
    Path(raw): Path<String>,
) -> Result<ApiOk<PrivateTxData>, ApiError> {
    // Transaction hashes are rendered lowercase, so normalise before matching.
    let postfix = remove_bad_chars(&raw).to_ascii_lowercase();

    let info = state
        .chain
        .info()
        .await
        .map_err(|e| on_chain_error(&e, "Cant get daemon info"))?;

    if let Err(why) = check_postfix(&postfix, total_transactions(&info)) {
        return Err(ApiError::fail(match why {
            PostfixRefusal::Length => format!(
                "Tx hash postfix not between {MIN_POSTFIX_LEN} and {MAX_POSTFIX_LEN} \
                 characters in length: {postfix}"
            ),
            PostfixRefusal::NotHex => format!("Tx hash postfix is not hex: {postfix}"),
            PostfixRefusal::TooLongToBeAnonymous { tx_count } => format!(
                "Tx hash postfix {postfix} is too long to be anonymous on a chain \
                 of {tx_count} transactions"
            ),
            PostfixRefusal::TooShortToServe { expected } => format!(
                "About {expected} transactions are expected to end with {postfix}, \
                 and this explorer serves at most {MAX_PRIVATE_TX_MATCHES}. \
                 Please use a longer postfix."
            ),
        }));
    }

    // monerod matches on bits, but the template it parses is a hash, so only
    // whole bytes can be expressed. An odd-length postfix searches one
    // character narrower and the surplus is filtered below.
    //
    // It is a *suffix*, so the narrower search drops the leading character and
    // keeps the trailing ones: "abc" searches "bc", not "ab". Taking the front
    // instead returns nothing at all, because no hash ends with "ab" by
    // coincidence of starting with it.
    let searched = whole_byte_suffix(&postfix);
    let request = GetTxidsLooseRequest::from_hex_suffix(searched)
        .ok_or_else(|| ApiError::fail(format!("Tx hash postfix is not hex: {postfix}")))?;

    let Some(found) = state
        .chain
        .txids_loose(&request)
        .await
        .map_err(|e| on_chain_error(&e, "Cant search for matching transactions"))?
    else {
        return Err(ApiError::error(
            "This daemon does not provide get_txids_loose, which the k-anonymous \
             lookup needs. It is in monerod master and release-v0.19 but in no \
             release build."
                .to_owned(),
        ));
    };

    // Filter the surplus an odd-length postfix pulled in, then cap.
    let matching: Vec<Hash32> = found
        .txids
        .iter()
        .filter(|t| t.ends_with(&postfix))
        .filter_map(|t| t.parse::<Hash32>().ok())
        .collect();

    if matching.len() as u64 > MAX_PRIVATE_TX_MATCHES {
        return Err(ApiError::fail(format!(
            "More than {MAX_PRIVATE_TX_MATCHES} transactions end with {postfix}. \
             Please use a longer postfix."
        )));
    }

    let fetched = state
        .chain
        .transactions(&matching)
        .await
        .map_err(|e| on_chain_error(&e, "Cant get matching transactions"))?;

    let mut txs = Vec::with_capacity(fetched.txs.len());
    for entry in &fetched.txs {
        let Ok(tx) = entry.parse_json() else { continue };
        // Rings are deliberately NOT expanded here. Expanding every ring of a
        // 500-transaction anonymity set would be thousands of RPC calls for a
        // caller who wants exactly one of them; the caller re-requests the one
        // it wanted through /api/transaction. The inputs are still listed --
        // they cost nothing and omitting them would report a spend as having
        // spent nothing.
        let inputs = unexpanded_inputs(&tx);
        let current = entry.block_height.saturating_add(entry.confirmations);
        txs.push(TxDetail::build(entry, &tx, &inputs, current));
    }

    Ok(ApiOk(PrivateTxData {
        missed_txs: fetched.missed,
        txs,
    }))
}

// ---------------------------------------------------------------------------
// /api/blocks/<start>/<end>   — k-anonymous block lookup
// ---------------------------------------------------------------------------

/// The widest block range this explorer will serve.
///
/// **A deliberate divergence from upstream devel, for the same reason the
/// `limit` clamp exists.** devel imposes no cap: it checks only that
/// `start <= end <= current_height`, which it can afford because it reads its
/// own database. Every block here costs the operator's daemon a call, so an
/// uncapped `/api/blocks/0/3000000` would be three million of them from one
/// unauthenticated request -- and a response to match, which has to be held in
/// memory to be sent.
///
/// 100 blocks is far wider than the anonymity set anyone needs — the point of
/// the endpoint is to hide *which* block was wanted, and a hundred candidates
/// does that — while bounding the cost at one header range, one `get_block`
/// per block that holds transactions, and one `get_transactions`. On a chain
/// where every block is full that is 102 calls; on a quiet one it is two.
///
/// **What it does not bound is bytes.** A hundred blocks is roughly ten
/// thousand mainnet transactions, and every one of them is fetched whole
/// before any of them is summarised. Measured against a mainnet daemon: one
/// such request takes about 10 seconds and holds about 40 MiB, and four at
/// once held 172 MiB. That number multiplies by `--max-concurrent`, whose
/// default of 128 would want some five gigabytes — far past the 512 MiB the
/// shipped systemd unit allows. An operator serving mainnet should size those
/// two against each other; see `deploy/oxblocks.service`.
pub const MAX_BLOCK_RANGE: u64 = 100;

pub async fn blocks_range(
    State(state): Shared,
    Path((start_raw, end_raw)): Path<(String, String)>,
) -> Result<ApiOk<Vec<BlockDetail>>, ApiError> {
    let start_s = remove_bad_chars(&start_raw);
    let end_s = remove_bad_chars(&end_raw);

    let start: u64 = start_s
        .parse()
        .map_err(|_| ApiError::fail(format!("Cant parse block number: {start_s}")))?;
    let end: u64 = end_s
        .parse()
        .map_err(|_| ApiError::fail(format!("Cant parse block number: {end_s}")))?;

    if start > end {
        return Err(ApiError::fail(
            "Invalid input: start height should be less than or equal to end height.".to_owned(),
        ));
    }

    let info = state
        .chain
        .info()
        .await
        .map_err(|e| on_chain_error(&e, "Cant get daemon info"))?;

    if end > info.height {
        return Err(ApiError::fail(format!(
            "Requested end height is higher than blockchain: {end}, {}",
            info.height
        )));
    }

    let span = end.saturating_sub(start).saturating_add(1);
    if span > MAX_BLOCK_RANGE {
        return Err(ApiError::fail(format!(
            "Requested {span} blocks; this explorer serves at most {MAX_BLOCK_RANGE} \
             per request because each one costs a call to the daemon."
        )));
    }

    let blocks = state
        .chain
        .blocks_in_range(start, end)
        .await
        .map_err(|e| on_chain_error(&e, &format!("Cant get blocks: {start} to {end}")))?;

    Ok(ApiOk(
        blocks
            .iter()
            .map(|b| block_detail(&b.header, &b.txs))
            .collect(),
    ))
}

/// Append a batch of transactions with their rings left unexpanded.
///
/// `/api/transactions/recent` answers with a *set* rather than a page, so
/// expanding every ring of it would be thousands of calls for a caller who
/// wants one of them and can re-request that one through `/api/transaction`.
/// Every transaction in the window shares one `current_height`, including the
/// pool ones, which is what upstream reports.
fn push_unexpanded(txs: &mut Vec<TxDetail>, entries: &[TxEntry], current_height: u64) {
    for entry in entries {
        let Ok(tx) = entry.parse_json() else { continue };
        let inputs = unexpanded_inputs(&tx);
        txs.push(TxDetail::build(entry, &tx, &inputs, current_height));
    }
}

// ---------------------------------------------------------------------------
// /api/transactions/recent
// ---------------------------------------------------------------------------

/// How many blocks back `/api/transactions/recent` reaches.
///
/// Matches devel. The endpoint exists so that a caller who wants a *recent*
/// transaction can take a window rather than name one — "a recent tx endpoint
/// the newest-guess cannot beat", in upstream's words: if everyone asks for
/// the same window, asking reveals nothing.
///
/// **The window is bounded; the pool beside it is not.** Every unconfirmed
/// transaction is listed, because counting them in `mempool_txs_no` and then
/// withholding them was a real bug here. monerod offers no paging on
/// `/get_transaction_pool` either, so the whole pool is fetched for the
/// `/mempool` page and for `/api/mempool` regardless — capping the listing
/// would shrink the response without shrinking the fetch. During a mempool
/// flood this is the most expensive endpoint here; see
/// `deploy/oxblocks.service`, which sizes `MemoryMax` against
/// `--max-concurrent` for exactly this family of requests.
pub const RECENT_BLOCKS: u64 = 30;

#[derive(Serialize)]
pub struct RecentData {
    current_height: u64,
    from_height: u64,
    mempool_txs_no: u64,
    to_height: u64,
    txs: Vec<TxDetail>,
}

pub async fn transactions_recent(State(state): Shared) -> Result<ApiOk<RecentData>, ApiError> {
    let info = state
        .chain
        .info()
        .await
        .map_err(|e| on_chain_error(&e, "Cant get daemon info"))?;

    let to_height = info.height.saturating_sub(1);
    let from_height = to_height.saturating_sub(RECENT_BLOCKS.saturating_sub(1));

    // The pool first: those are more recent than any mined transaction, and a
    // caller reaching for this endpoint is reaching for a recent one. Counting
    // them in `mempool_txs_no` while leaving them out of `txs` would name a
    // set and then withhold it.
    let pool = state.chain.mempool().await.ok();
    let mut txs = Vec::new();
    let mut mempool_txs_no = 0;
    for entry in pool.iter().flat_map(|p| p.transactions.iter()) {
        let Ok(tx) = entry.parse_json() else { continue };
        let inputs = unexpanded_inputs(&tx);
        txs.push(TxDetail::build_pool(entry, &tx, &inputs, info.height));
        mempool_txs_no += 1;
    }

    if let Ok(window) = state.chain.blocks_in_range(from_height, to_height).await {
        for block in &window {
            push_unexpanded(&mut txs, &block.txs, info.height);
        }
    }

    Ok(ApiOk(RecentData {
        current_height: info.height,
        from_height,
        mempool_txs_no,
        to_height,
        txs,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]

    use super::*;

    // Both bugs below were found by diffing against a live upstream devel
    // build on the same chain, not by reading the code.

    /// A transaction still in the pool has no block, so no block timestamp --
    /// monerod puts the time it arrived in `received_timestamp` instead.
    /// Reading `block_timestamp` regardless dated every unconfirmed
    /// transaction 1970-01-01, which was found by comparing against upstream
    /// on a chain that, unlike the earlier one, had a non-empty pool.
    #[test]
    fn a_pool_transaction_is_dated_by_when_it_arrived() {
        let entry: TxEntry = serde_json::from_value(serde_json::json!({
            "tx_hash": "37".repeat(32),
            "in_pool": true,
            "received_timestamp": 1_789_838_436u64,
            "block_timestamp": 0,
            "block_height": 0,
        }))
        .expect("a pool tx entry");
        let tx: monerod_rpc::types::TxJson = serde_json::from_value(serde_json::json!({
            "version": 2, "unlock_time": 0, "vin": [], "vout": [], "extra": [],
        }))
        .expect("a transaction");

        let detail = TxDetail::build(&entry, &tx, &[], 137_082);
        let rendered = serde_json::to_value(&detail).expect("serialises");
        assert_eq!(rendered["timestamp"], 1_789_838_436u64);
        assert_eq!(rendered["timestamp_utc"], "2026-09-19 17:20:36");
        assert_eq!(
            rendered["confirmations"], 0,
            "the pool has no confirmations"
        );
    }

    /// An odd-length postfix must narrow to its TRAILING bytes. Taking the
    /// leading ones returned an empty set while still reporting success --
    /// upstream found 37 matches for "abc" where this returned 0.
    #[test]
    fn an_odd_postfix_narrows_to_its_trailing_bytes() {
        assert_eq!(whole_byte_suffix("abc"), "bc");
        assert_eq!(whole_byte_suffix("abcde"), "bcde");
        // Even lengths are already whole bytes and are untouched.
        assert_eq!(whole_byte_suffix("ab"), "ab");
        assert_eq!(whole_byte_suffix("abcd"), "abcd");
        assert_eq!(whole_byte_suffix(""), "");
    }

    /// The anonymity rule, which is the entire privacy property of the
    /// endpoint. Numbers are upstream's, checked against the same chain.
    #[test]
    fn the_anonymity_rule_matches_upstream() {
        // The local testnet: 134,875 transactions.
        const CHAIN: u64 = 134_875;
        assert!(check_postfix("00", CHAIN).is_ok());
        assert!(check_postfix("abc", CHAIN).is_ok());
        // 134875 >> 16 == 2, below the floor of 20, so four characters would
        // identify the transaction rather than hide it.
        assert!(matches!(
            check_postfix("abcd", CHAIN),
            Err(PostfixRefusal::TooLongToBeAnonymous { .. })
        ));

        // Mainnet-scale: upstream's comment says this permits 5 and refuses 6.
        const MAINNET: u64 = 67_000_000;
        assert!(check_postfix("abcde", MAINNET).is_ok());
        assert!(matches!(
            check_postfix("abcdef", MAINNET),
            Err(PostfixRefusal::TooLongToBeAnonymous { .. })
        ));
    }

    #[test]
    fn postfix_length_and_alphabet_are_enforced() {
        const CHAIN: u64 = 134_875;
        assert!(matches!(
            check_postfix("0", CHAIN),
            Err(PostfixRefusal::Length)
        ));
        assert!(matches!(
            check_postfix(&"0".repeat(13), CHAIN),
            Err(PostfixRefusal::Length)
        ));
        assert!(matches!(
            check_postfix("zz", CHAIN),
            Err(PostfixRefusal::NotHex)
        ));
    }

    /// The shift is bounded by MAX_POSTFIX_LEN, so it can never reach the
    /// width of the integer -- 12 characters is 48 bits. On an absurdly large
    /// chain the arithmetic must reach an answer rather than panicking on the
    /// way, and the answer is that even the longest postfix this explorer
    /// accepts still names 65,535 expected transactions there, which is more
    /// than it will expand.
    #[test]
    fn the_arithmetic_survives_an_enormous_chain() {
        assert!(u32::try_from(MAX_POSTFIX_LEN * 4).is_ok_and(|b| b < 64));
        assert!(matches!(
            check_postfix(&"a".repeat(MAX_POSTFIX_LEN), u64::MAX),
            Err(PostfixRefusal::TooShortToServe { expected }) if expected == u64::MAX >> 48
        ));
    }

    /// The guard that keeps a cheap request from becoming an expensive scan.
    ///
    /// `get_txids_loose` walks the whole transaction index. A two-character
    /// postfix on mainnet matches about a quarter of a million transactions --
    /// far more than this explorer will expand -- so the request is refused
    /// either way. Refusing on the arithmetic means the daemon is never asked.
    #[test]
    fn a_postfix_whose_set_is_too_large_to_serve_is_refused_before_the_daemon() {
        const MAINNET: u64 = 67_000_000;
        assert!(matches!(
            check_postfix("ab", MAINNET),
            Err(PostfixRefusal::TooShortToServe { expected }) if expected == MAINNET >> 8
        ));
        assert!(matches!(
            check_postfix("abcd", MAINNET),
            Err(PostfixRefusal::TooShortToServe { .. })
        ));
        // And the band is not empty: exactly one length fits mainnet.
        assert!(check_postfix("abcde", MAINNET).is_ok());
    }

    /// An empty chain must refuse everything rather than divide its way into
    /// permitting a postfix that identifies the only transaction there is.
    #[test]
    fn an_empty_chain_refuses_every_postfix() {
        for len in MIN_POSTFIX_LEN..=MAX_POSTFIX_LEN {
            assert!(matches!(
                check_postfix(&"a".repeat(len), 0),
                Err(PostfixRefusal::TooLongToBeAnonymous { .. })
            ));
        }
    }
}
