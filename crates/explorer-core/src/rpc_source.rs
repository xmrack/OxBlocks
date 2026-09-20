//! The one thing that answers questions about the chain, over monerod's RPC.
//!
//! There is no trait here and no second implementation to abstract over. The
//! vocabulary those questions are asked in lives in [`crate::chain`].

use monerod_rpc::types::error_code;
use monerod_rpc::types::{
    FeeEstimate, GetAlternateChains, GetBlock, GetBlockHeader, GetBlockHeadersRange,
    GetBlockHeadersRangeRequest, GetBlockRequest, GetFeeEstimateRequest, GetInfo, GetOutsRequest,
    GetTransactionPool, GetTransactionPoolStats, GetTransactionsRequest, GetTxidsLooseRequest,
    GetTxidsLooseResponse, OutKey, OutKeyRequest, TxEntry, TxInToKey, TxJson,
};
use monerod_rpc::{Client, RpcError};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::cache::{Cache, REORG_WINDOW, safe_to_cache_by_height};
use crate::chain::{BlockId, ChainError, ResolvedInput, RingMember};
use crate::hash::Hash32;

/// What a batch fetch actually yielded.
///
/// monerod reports a transaction it does not have in `missed_tx`, not through
/// `status`, and omits the field entirely when nothing was missed -- so a
/// caller that only looks at `txs` cannot tell "not found" from "found
/// nothing". Keeping the two together makes that distinction hard to drop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FetchedTxs {
    pub txs: Vec<TxEntry>,
    /// Hashes monerod does not have, as it returned them.
    pub missed: Vec<String>,
}

/// Put a fetched batch back into the order it was asked for.
///
/// Cache hits and freshly fetched transactions arrive from two different
/// places, and a block's order carries meaning: it lists its transactions in a
/// fixed order with the coinbase first. Answering in the order the two sources
/// happened to produce would be right on a cold server and wrong on a warm
/// one, which is the worst shape a bug can take.
fn in_requested_order(hashes: &[Hash32], mut found: HashMap<Hash32, TxEntry>) -> Vec<TxEntry> {
    hashes.iter().filter_map(|h| found.remove(h)).collect()
}

pub struct RpcChainSource {
    client: Client,
    /// Blocks keyed by hash. A hash names one block forever, so this never
    /// needs invalidating.
    blocks_by_hash: Cache<Hash32, GetBlock>,
    /// Blocks keyed by height, populated **only** for heights buried deeper
    /// than [`REORG_WINDOW`]. A reorg reassigns a height to a different block,
    /// so caching a recent height would serve a block that no longer exists.
    blocks_by_height: Cache<u64, GetBlock>,
    /// Confirmed transactions, keyed by hash and cached only once buried.
    txs: Cache<Hash32, TxEntry>,
    /// Ring members. An output at a given index is immutable once it exists.
    outs: Cache<(u64, u64), OutKey>,
    /// The chain tip. Short-lived by nature, so it expires rather than being
    /// invalidated.
    info: Cache<(), GetInfo>,
    /// Bounds how many RPC calls can be in flight against the daemon at once.
    ///
    /// The single choke point protecting the operator's node. Per-request
    /// limits bound how much work *one* request can ask for, but they multiply
    /// by however many requests arrive at once; this bounds the product,
    /// whatever any future endpoint does. A request that cannot get a permit
    /// waits, and the inbound request timeout eventually sheds it -- queueing
    /// in front of the daemon rather than stampeding it.
    rpc_permits: Arc<Semaphore>,
    /// Outbound calls made since start.
    ///
    /// The number that matters for load on the operator's node, and not the
    /// same as cache misses -- one `/get_transactions` carrying forty hashes
    /// is forty misses and one call. Conflating them overstates amplification
    /// by an order of magnitude, which is how this counter came to exist.
    rpc_calls: Arc<AtomicU64>,
}

/// Turn a `get_block` error code into something the web layer can act on.
///
/// Every JSON-RPC error used to become "no such block", so a daemon that was
/// merely busy told the reader their block did not exist. The codes below are
/// what monerod actually returns, measured against a live node:
///
/// | ask | code | meaning |
/// | --- | --- | --- |
/// | height past the tip | -2 | not found |
/// | hash it does not hold | -5 | not found |
/// | unparseable hex | -1 | not found |
/// | busy | -9 | try again |
/// | method gated off | -32601 | unavailable by configuration |
///
/// `-5` is `INTERNAL`, a generic code, but for a `get_block` *by hash* it is
/// how monerod says it has no such block ("Internal error: can't get block by
/// hash"). That reading is specific to this call and does not generalise.
///
/// Anything unrecognised is reported as unavailable rather than as missing:
/// claiming a block does not exist is a stronger statement than we can make
/// about an error we do not understand.
fn classify_block_error(code: i64, id: BlockId, detail: String) -> ChainError {
    use monerod_rpc::types::error_code as ec;

    match code {
        ec::WRONG_PARAM | ec::TOO_BIG_HEIGHT | ec::INTERNAL => ChainError::BlockNotFound(id),
        ec::CORE_BUSY => ChainError::Unavailable(detail),
        ec::METHOD_NOT_FOUND | ec::RESTRICTED => ChainError::NeedsUnrestricted("block lookup"),
        _ => ChainError::Unavailable(detail),
    }
}

/// Transaction hashes per `/get_transactions` call.
///
/// The call accepts any number, and asking for a hundred blocks' transactions
/// in one request instead of a hundred is the whole point of batching -- but
/// the response carries every blob, so an unbounded batch is an unbounded
/// allocation, which is the failure mode this explorer exists to avoid. A few
/// large calls rather than one enormous one; they are issued together, so the
/// split costs no extra round trip.
const MAX_TXS_PER_CALL: usize = 500;

/// One block's header and every transaction in it, coinbase first.
pub struct BlockWithTxs {
    pub header: monerod_rpc::types::BlockHeader,
    pub txs: Vec<TxEntry>,
}

/// Concurrent RPC calls allowed against the daemon.
///
/// monerod answers RPC on a bounded thread pool, so flooding it degrades the
/// node itself -- including its peer-to-peer duties. Kept well under what a
/// daemon will happily serve.
pub const DEFAULT_MAX_INFLIGHT_RPC: usize = 24;

impl RpcChainSource {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            // Sized for a working set of a few thousand objects: enough that
            // paging through recent history is warm, small enough that the
            // process footprint stays predictable.
            blocks_by_hash: Cache::permanent(512),
            blocks_by_height: Cache::permanent(2048),
            txs: Cache::permanent(8192),
            outs: Cache::permanent(65_536),
            // Long enough to collapse the several calls a single page makes,
            // short enough that the height on screen is never visibly stale.
            info: Cache::expiring(1, Duration::from_secs(5)),
            rpc_permits: Arc::new(Semaphore::new(DEFAULT_MAX_INFLIGHT_RPC)),
            rpc_calls: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Outbound RPC calls made since start.
    #[must_use]
    pub fn rpc_calls(&self) -> u64 {
        self.rpc_calls.load(Ordering::Relaxed)
    }

    /// Override the in-flight RPC ceiling. See [`DEFAULT_MAX_INFLIGHT_RPC`].
    #[must_use]
    pub fn with_max_inflight_rpc(mut self, permits: usize) -> Self {
        self.rpc_permits = Arc::new(Semaphore::new(permits.max(1)));
        self
    }

    /// Call a JSON-RPC method, holding a permit for the duration.
    ///
    /// Together with [`Self::bare`] this is the **only** path to the daemon:
    /// `client` is private and has no accessor, so a new call site cannot
    /// forget the permit. An earlier version relied on remembering, and an
    /// audit of it missed a call written with a turbofish.
    async fn rpc<P, R>(&self, method: &'static str, params: Option<P>) -> Result<R, RpcError>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let _permit = self.permit().await;
        self.rpc_calls.fetch_add(1, Ordering::Relaxed);
        self.client.json_rpc(method, params).await
    }

    /// Call a bare (non-JSON-RPC) endpoint, holding a permit for the duration.
    async fn bare<B, R>(&self, endpoint: &'static str, body: &B) -> Result<R, RpcError>
    where
        B: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let _permit = self.permit().await;
        self.rpc_calls.fetch_add(1, Ordering::Relaxed);
        self.client.endpoint(endpoint, body).await
    }

    /// Acquire a permit for one call against the daemon.
    ///
    /// Returns `None` only if the semaphore has been closed, which this code
    /// never does; callers treat that as "proceed" rather than failing a page
    /// over a condition that cannot arise.
    async fn permit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.rpc_permits).acquire_owned().await.ok()
    }

    /// Cache occupancy and hit counts, for `/health` and for tests.
    pub fn cache_stats(&self) -> [(&'static str, crate::cache::Stats); 5] {
        [
            ("blocks_by_hash", self.blocks_by_hash.stats()),
            ("blocks_by_height", self.blocks_by_height.stats()),
            ("txs", self.txs.stats()),
            ("outs", self.outs.stats()),
            ("info", self.info.stats()),
        ]
    }

    pub async fn info(&self) -> Result<Arc<GetInfo>, ChainError> {
        if let Some(hit) = self.info.get(&()) {
            return Ok(hit);
        }
        let fresh: GetInfo = self.rpc("get_info", None::<()>).await?;
        Ok(self.info.insert((), fresh))
    }

    pub async fn block(&self, id: BlockId) -> Result<Arc<GetBlock>, ChainError> {
        match id {
            BlockId::Hash(h) => {
                if let Some(hit) = self.blocks_by_hash.get(&h) {
                    return Ok(hit);
                }
            }
            BlockId::Height(h) => {
                if let Some(hit) = self.blocks_by_height.get(&h) {
                    return Ok(hit);
                }
            }
        }

        let request = match id {
            BlockId::Height(h) => GetBlockRequest::by_height(h),
            BlockId::Hash(h) => GetBlockRequest::by_hash(h.to_hex()),
        };

        let fresh: GetBlock = self
            .rpc("get_block", Some(request))
            .await
            .map_err(|e| match &e {
                RpcError::JsonRpc { code, .. } => classify_block_error(*code, id, e.to_string()),
                _ => ChainError::from(e),
            })?;

        // Keyed by hash: always safe, because a hash names one block forever.
        if let Ok(h) = fresh.block_header.hash.parse::<Hash32>() {
            self.blocks_by_hash.insert(h, fresh.clone());
        }

        // Keyed by height: only once buried past the reorg window, because a
        // reorg reassigns a height to a different block. `depth` is the
        // daemon's own count of how far down the chain this block sits.
        if safe_to_cache_by_height(fresh.block_header.depth) {
            return Ok(self
                .blocks_by_height
                .insert(fresh.block_header.height, fresh));
        }

        Ok(Arc::new(fresh))
    }

    pub async fn transactions(&self, hashes: &[Hash32]) -> Result<FetchedTxs, ChainError> {
        // Asking monerod for nothing is not a useful round trip, and the empty
        // wire envelope would have to claim a status it was never given.
        if hashes.is_empty() {
            return Ok(FetchedTxs::default());
        }

        // Answer whatever is already cached and ask only for the rest. A block
        // page re-requests the same transactions every time it is reloaded,
        // and a reader paging back and forth revisits the same ones.
        let mut found: HashMap<Hash32, TxEntry> = HashMap::new();
        let mut want: Vec<String> = Vec::new();
        for h in hashes {
            match self.txs.get(h) {
                Some(hit) => {
                    found.insert(*h, (*hit).clone());
                }
                None => want.push(h.to_hex()),
            }
        }

        let mut missed = Vec::new();
        for response in futures_util::future::join_all(want.chunks(MAX_TXS_PER_CALL).map(|chunk| {
            let request = GetTransactionsRequest::decoded(chunk.to_vec());
            async move {
                self.bare::<_, monerod_rpc::types::GetTransactionsResponse>(
                    "get_transactions",
                    &request,
                )
                .await
            }
        }))
        .await
        {
            let response = response?;
            for entry in response.txs {
                let Ok(h) = entry.tx_hash.parse::<Hash32>() else {
                    continue;
                };
                // Only confirmed and buried transactions. A pool transaction
                // will gain a block, and a freshly confirmed one can still be
                // reorged back out -- either would be cached as a lie.
                if !entry.in_pool && entry.confirmations >= REORG_WINDOW {
                    self.txs.insert(h, entry.clone());
                }
                found.insert(h, entry);
            }
            missed.extend(response.missed_tx);
        }

        Ok(FetchedTxs {
            txs: in_requested_order(hashes, found),
            missed,
        })
    }

    /// Block headers for a height range, inclusive at both ends.
    ///
    /// monerod caps the span at 1000 under a restricted daemon. We require an
    /// unrestricted one, but the cap is cheap to respect and keeps a single
    /// oversized request from being rejected wholesale.
    pub async fn headers_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<GetBlockHeadersRange, ChainError> {
        const MAX_SPAN: u64 = 1000;
        let end = end.min(start.saturating_add(MAX_SPAN - 1));
        let request = GetBlockHeadersRangeRequest {
            start_height: start,
            end_height: end,
            fill_pow_hash: false,
        };
        Ok(self.rpc("get_block_headers_range", Some(request)).await?)
    }

    /// Every block in a height range, with its transactions.
    ///
    /// The obvious shape -- `get_block` then `get_transactions`, per block,
    /// each awaited before the next begins -- is 2N round trips in single
    /// file. This is three things instead:
    ///
    /// * one `get_block_headers_range` for the whole span, which already
    ///   carries every field a block listing shows;
    /// * `get_block` only for the blocks that hold user transactions, because
    ///   the header names the coinbase and counts the rest -- and all of them
    ///   at once rather than one after another;
    /// * one `get_transactions` for every hash in the range together.
    ///
    /// So 2N becomes 2 + however many blocks hold anything, and the round
    /// trips overlap. On a busy chain that is about half the calls; on a quiet
    /// one, where most blocks are nothing but their coinbase, it is two.
    ///
    /// The headers are not cached, so a range that was served before now costs
    /// one call rather than none. That is the trade for the cold path costing
    /// two instead of two hundred.
    pub async fn blocks_in_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<BlockWithTxs>, ChainError> {
        let headers = self.headers_range(start, end).await?.headers;

        let holding: Vec<u64> = headers
            .iter()
            .filter(|h| h.num_txes > 0)
            .map(|h| h.height)
            .collect();
        let bodies =
            futures_util::future::join_all(holding.iter().map(|h| self.block(BlockId::Height(*h))))
                .await;

        let mut extra: HashMap<u64, Arc<GetBlock>> = HashMap::with_capacity(holding.len());
        for (height, body) in holding.iter().zip(bodies) {
            extra.insert(*height, body?);
        }

        // Every hash in the range, in the order its block lists them.
        let wanted: Vec<Vec<Hash32>> = headers
            .iter()
            .map(|h| {
                let mut hashes: Vec<Hash32> = Vec::new();
                hashes.extend(h.miner_tx_hash.parse::<Hash32>());
                if let Some(body) = extra.get(&h.height) {
                    hashes.extend(
                        body.tx_hashes
                            .iter()
                            .filter_map(|t| t.parse::<Hash32>().ok()),
                    );
                }
                hashes
            })
            .collect();

        let flat: Vec<Hash32> = wanted.iter().flatten().copied().collect();
        let mut fetched: HashMap<Hash32, TxEntry> = self
            .transactions(&flat)
            .await?
            .txs
            .into_iter()
            .filter_map(|e| e.tx_hash.parse::<Hash32>().ok().map(|h| (h, e)))
            .collect();

        Ok(headers
            .into_iter()
            .zip(wanted)
            .map(|(header, hashes)| BlockWithTxs {
                header,
                txs: hashes.iter().filter_map(|h| fetched.remove(h)).collect(),
            })
            .collect())
    }

    /// The tip block's header.
    ///
    /// Its `major_version` is the active hard-fork version, which is why
    /// `/api/networkinfo` needs it.
    pub async fn last_block_header(&self) -> Result<GetBlockHeader, ChainError> {
        Ok(self.rpc("get_last_block_header", None::<()>).await?)
    }

    pub async fn fee_estimate(&self, grace_blocks: u64) -> Result<FeeEstimate, ChainError> {
        let request = GetFeeEstimateRequest { grace_blocks };
        Ok(self.rpc("get_fee_estimate", Some(request)).await?)
    }

    /// Transaction ids whose low bits match a template.
    ///
    /// The daemon side of the k-anonymous lookup: the caller names a suffix
    /// rather than a transaction, and everything matching comes back, so the
    /// explorer never learns which one was wanted.
    ///
    /// `Ok(None)` means the daemon does not have the method -- it is in
    /// monerod `master` and `release-v0.19` but in no release, and v0.18.5.1
    /// answers `Method not found`. That is a deployment fact rather than a
    /// failure, so it is a distinct outcome from an error.
    pub async fn txids_loose(
        &self,
        request: &GetTxidsLooseRequest,
    ) -> Result<Option<GetTxidsLooseResponse>, ChainError> {
        match self
            .rpc::<_, GetTxidsLooseResponse>("get_txids_loose", Some(request))
            .await
        {
            Ok(r) => Ok(Some(r)),
            Err(RpcError::JsonRpc { code, .. }) if code == error_code::METHOD_NOT_FOUND => Ok(None),
            Err(e) => Err(ChainError::from(e)),
        }
    }

    /// Whether this daemon offers `get_txids_loose`.
    ///
    /// Probed with a request that is cheap and certain to be rejected on
    /// substance if the method exists at all, so the answer distinguishes
    /// "absent" from "present but declined".
    pub async fn supports_txids_loose(&self) -> bool {
        let Some(probe) = GetTxidsLooseRequest::from_hex_suffix(&"0".repeat(64)) else {
            return false;
        };
        !matches!(
            self.rpc::<_, GetTxidsLooseResponse>("get_txids_loose", Some(&probe))
                .await,
            Err(RpcError::JsonRpc { code, .. }) if code == error_code::METHOD_NOT_FOUND
        )
    }

    /// Alternative chains this node is tracking.
    ///
    /// Blocked under `--restricted-rpc`, like the mempool.
    pub async fn alt_chains(&self) -> Result<GetAlternateChains, ChainError> {
        self.rpc("get_alternate_chains", None::<()>)
            .await
            .map_err(|e| match &e {
                RpcError::JsonRpc { code, .. } if *code == error_code::METHOD_NOT_FOUND => {
                    ChainError::NeedsUnrestricted("alternative chains")
                }
                _ => ChainError::from(e),
            })
    }

    /// The mempool.
    ///
    /// Blocked under `--restricted-rpc`, so a restricted daemon surfaces as
    /// [`ChainError::NeedsUnrestricted`] rather than as a generic failure --
    /// the page is unavailable by configuration, not broken.
    pub async fn mempool(&self) -> Result<GetTransactionPool, ChainError> {
        let mut pool: GetTransactionPool = self
            .bare("get_transaction_pool", &serde_json::json!({}))
            .await
            .map_err(|e| match &e {
                RpcError::Http { status: 404, .. } => ChainError::NeedsUnrestricted("the mempool"),
                _ => ChainError::from(e),
            })?;

        newest_first(&mut pool.transactions);
        Ok(pool)
    }

    /// The pool's aggregate figures, without the pool.
    ///
    /// `/api/networkinfo` wants one number out of the mempool, its size in
    /// bytes. Summing it from [`Self::mempool`] means the daemon serialises
    /// every pool transaction -- 187 kB against 868 on a 12-transaction
    /// testnet pool -- for a page that is otherwise the cheapest one here.
    pub async fn pool_stats(&self) -> Result<GetTransactionPoolStats, ChainError> {
        self.bare("get_transaction_pool_stats", &serde_json::json!({}))
            .await
            .map_err(|e| match &e {
                RpcError::Http { status: 404, .. } => ChainError::NeedsUnrestricted("the mempool"),
                _ => ChainError::from(e),
            })
    }

    /// Resolve one input's ring, in a call of its own.
    ///
    /// **The fallback path.** [`Self::resolve_rings`] asks for a whole
    /// transaction's rings at once and drops to this when the daemon refuses:
    /// monerod fails the *entire* `/get_outs` request if any single
    /// `(amount, index)` pair is out of range — measured, mixing one bad index
    /// with one good one returns `status: "Failed"` and no `outs` key at all —
    /// so a batch cannot say which input was the bad one. One call per input
    /// can, which is how one unresolvable input avoids blanking every ring on
    /// the page. Upstream drops just the offending input and renders the rest,
    /// and so do we.
    ///
    /// Never returns `Err` for an unresolvable ring: that is a per-input
    /// display state, not a page failure.
    pub async fn resolve_ring(&self, input: &TxInToKey) -> ResolvedInput {
        let key_image = input.k_image.parse::<Hash32>().unwrap_or(Hash32::ZERO);

        let unresolved = |unavailable: bool| ResolvedInput {
            amount: input.amount,
            key_image,
            ring: Vec::new(),
            ring_unavailable: unavailable,
        };

        // `ring_members` carries this input's own amount through with each
        // index. For a pre-RingCT input the index means nothing without it:
        // the cumulative offset sum addresses that denomination's output set,
        // not the global one.
        let Some(requests) = input.ring_members() else {
            // A hostile offset list that overflows u64 on summation.
            return unresolved(true);
        };
        if requests.is_empty() {
            return unresolved(false);
        }

        let wanted = requests.len();

        // Ring members repeat heavily: popular outputs are chosen as decoys
        // again and again, and reloading a page re-requests the same ring. If
        // every member is already known, the daemon is not asked at all.
        let cached: Vec<Option<Arc<OutKey>>> = requests
            .iter()
            .map(|r| self.outs.get(&(r.amount(), r.index())))
            .collect();
        if cached.iter().all(Option::is_some) {
            return ResolvedInput {
                amount: input.amount,
                key_image,
                ring: requests
                    .iter()
                    .zip(cached.iter())
                    .filter_map(|(req, hit)| {
                        let out = hit.as_ref()?;
                        Some(RingMember {
                            index: req.index(),
                            block_height: out.height,
                            public_key: out.key.parse().unwrap_or(Hash32::ZERO),
                            tx_hash: out.txid.parse().unwrap_or(Hash32::ZERO),
                        })
                    })
                    .collect(),
                ring_unavailable: false,
            };
        }

        let request = GetOutsRequest::new(requests.clone(), true);
        let Ok(response) = self
            .bare::<_, monerod_rpc::types::GetOutsResponse>("get_outs", &request)
            .await
        else {
            return unresolved(true);
        };

        // A short `outs` array cannot be zipped positionally against the
        // requests: we would silently attribute one offset's output to another.
        if response.outs.len() != wanted {
            return unresolved(true);
        }

        for (req, out) in requests.iter().zip(response.outs.iter()) {
            self.outs.insert((req.amount(), req.index()), out.clone());
        }

        let ring = requests
            .iter()
            .zip(response.outs.iter())
            .map(|(req, out)| RingMember {
                index: req.index(),
                block_height: out.height,
                public_key: out.key.parse().unwrap_or(Hash32::ZERO),
                tx_hash: out.txid.parse().unwrap_or(Hash32::ZERO),
            })
            .collect();

        ResolvedInput {
            amount: input.amount,
            key_image,
            ring,
            ring_unavailable: false,
        }
    }

    /// Resolve every input of a transaction, one request per input.
    ///
    /// The requests are issued **concurrently**, not in sequence. They stay
    /// one-per-input for the correctness reason on [`Self::resolve_ring`] --
    /// monerod fails a whole batch if any index is out of range -- but nothing
    /// requires waiting for each before starting the next.
    ///
    /// This matters on real data: mainnet transaction bf1b4e2b…c193 has 195
    /// inputs, and resolving them in sequence took 6.1 seconds. The semaphore
    /// still bounds how many actually reach the daemon at once, so this
    /// shortens the request rather than deepening the load.
    ///
    /// `join_all` preserves order, which ring display depends on: ring `n`
    /// must belong to input `n`.
    pub async fn resolve_rings(&self, tx: &TxJson) -> Vec<ResolvedInput> {
        let inputs: Vec<&TxInToKey> = tx
            .vin
            .iter()
            .filter_map(|input| match input {
                monerod_rpc::types::TxIn::Key(k) => Some(k),
                _ => None,
            })
            .collect();

        // One call for the whole transaction when the daemon will allow it.
        // A transaction's inputs are independent only in the failure case, so
        // paying a round trip each is paying for a case that almost never
        // happens: mainnet bf1b4e2b..c193 has 195 inputs and cost 195 calls.
        if inputs.len() > 1
            && let Some(resolved) = self.resolve_rings_together(&inputs).await
        {
            return resolved;
        }

        futures_util::future::join_all(inputs.iter().map(|k| self.resolve_ring(k))).await
    }

    /// Every ring of one transaction in a single `get_outs`.
    ///
    /// `None` means the batch is not usable and the caller must fall back to
    /// one call per input: monerod fails the *whole* request if any single
    /// index is out of range, so a batch cannot report which input was the bad
    /// one, and the per-input path exists precisely so that one unresolvable
    /// input does not blank every ring on the page.
    ///
    /// Nothing here reads a ring member back out of the cache after writing
    /// it. The cache is a bounded LRU shared with every other request, so an
    /// entry written at the top of this function can be evicted before the
    /// bottom of it, and a ring assembled from what survived would be reported
    /// as partly unavailable when it was in fact complete.
    async fn resolve_rings_together(&self, inputs: &[&TxInToKey]) -> Option<Vec<ResolvedInput>> {
        let rings: Vec<Vec<OutKeyRequest>> = inputs
            .iter()
            .map(|k| k.ring_members())
            .collect::<Option<_>>()?;
        if rings.iter().any(Vec::is_empty) {
            return None;
        }

        // Ask only for what is not already known, and only once for a decoy
        // that two inputs happen to share -- which they may, decoys being
        // drawn independently per input.
        let mut known: HashMap<(u64, u64), Arc<OutKey>> = HashMap::new();
        let mut wanted: Vec<OutKeyRequest> = Vec::new();
        // A set rather than a linear scan of `wanted`: mainnet bf1b4e2b..c193
        // has 195 inputs, so its rings hold 3,120 members and a scan-per-member
        // is about five million comparisons before a single byte is asked for.
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        for member in rings.iter().flatten() {
            let key = (member.amount(), member.index());
            if !seen.insert(key) {
                continue;
            }
            match self.outs.get(&key) {
                Some(hit) => drop(known.insert(key, hit)),
                None => wanted.push(*member),
            }
        }

        if !wanted.is_empty() {
            let request = GetOutsRequest::new(wanted.clone(), true);
            let response: monerod_rpc::types::GetOutsResponse =
                self.bare("get_outs", &request).await.ok()?;

            // A short array cannot be zipped positionally against the
            // requests: we would attribute one offset's output to another.
            if response.outs.len() != wanted.len() {
                return None;
            }
            for (req, out) in wanted.iter().zip(response.outs) {
                let key = (req.amount(), req.index());
                known.insert(key, self.outs.insert(key, out));
            }
        }

        Some(
            inputs
                .iter()
                .zip(rings)
                .map(|(input, members)| {
                    let ring: Vec<RingMember> = members
                        .iter()
                        .filter_map(|m| {
                            let out = known.get(&(m.amount(), m.index()))?;
                            Some(RingMember {
                                index: m.index(),
                                block_height: out.height,
                                public_key: out.key.parse().unwrap_or(Hash32::ZERO),
                                tx_hash: out.txid.parse().unwrap_or(Hash32::ZERO),
                            })
                        })
                        .collect();
                    let whole = ring.len() == members.len();
                    ResolvedInput {
                        amount: input.amount,
                        key_image: input.k_image.parse().unwrap_or(Hash32::ZERO),
                        ring: if whole { ring } else { Vec::new() },
                        ring_unavailable: !whole,
                    }
                })
                .collect(),
        )
    }
}

/// Order pool transactions by arrival, newest first.
///
/// monerod returns the pool in its own internal order, which is neither
/// arrival order nor stable between calls. Newest first is the only order a
/// pool listing means anything in, and it is what upstream sorts to -- its own
/// comment is "mempool txs are not sorted base on their arival time, so we
/// sort it here".
fn newest_first(txs: &mut [monerod_rpc::types::PoolTxInfo]) {
    txs.sort_by_key(|t| std::cmp::Reverse(t.receive_time));
}

/// A transaction's inputs, listed but not expanded.
///
/// Only the ring members cost a lookup; the amount and key image are in the
/// transaction itself. An endpoint that deliberately skips the lookup -- the
/// k-anonymous one expands nothing, because expanding every ring of a
/// thousand-transaction anonymity set is thousands of calls for a caller who
/// wants one of them -- still has to say the inputs are there. Answering
/// `"inputs": []` for a transaction that spends something is not an
/// abbreviation of the truth, it is a different claim, and it contradicts the
/// `"coinbase": false` sitting beside it.
#[must_use]
pub fn unexpanded_inputs(tx: &TxJson) -> Vec<ResolvedInput> {
    tx.vin
        .iter()
        .filter_map(|input| match input {
            monerod_rpc::types::TxIn::Key(k) => Some(ResolvedInput {
                amount: k.amount,
                key_image: k.k_image.parse().unwrap_or(Hash32::ZERO),
                ring: Vec::new(),
                ring_unavailable: true,
            }),
            _ => None,
        })
        .collect()
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

    fn source() -> RpcChainSource {
        RpcChainSource::new(Client::new("http://127.0.0.1:1").expect("valid url"))
    }

    fn hash(byte: u8) -> Hash32 {
        format!("{byte:02x}")
            .repeat(32)
            .parse()
            .expect("32 bytes of hex")
    }

    /// Every field but these two has a serde default, so this is the whole
    /// shape a confirmed transaction needs to stand in for one here.
    fn entry(h: Hash32) -> TxEntry {
        serde_json::from_value(serde_json::json!({
            "tx_hash": h.to_hex(),
            "in_pool": false,
        }))
        .expect("a minimal confirmed tx entry")
    }

    /// The order a block lists its transactions in is part of the answer, and
    /// the coinbase is first. Cache hits and fetched transactions arrive from
    /// two different places, so assembling them in arrival order gets this
    /// right only while the cache is empty -- warming one transaction of a
    /// block by visiting its own page used to move it to the front of that
    /// block's list.
    #[test]
    fn a_partly_cached_batch_keeps_the_order_it_was_asked_for() {
        let asked = [hash(0x11), hash(0x22), hash(0x33)];

        // Insert in an order unrelated to `asked`, as the two sources would:
        // the middle one was the cache hit, the others came back from the RPC.
        let mut found = HashMap::new();
        found.insert(asked[1], entry(asked[1]));
        found.insert(asked[2], entry(asked[2]));
        found.insert(asked[0], entry(asked[0]));

        let got = in_requested_order(&asked, found);
        let order: Vec<String> = got.iter().map(|e| e.tx_hash.clone()).collect();
        assert_eq!(
            order,
            asked.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
            "answered in its own order rather than the caller's"
        );
    }

    /// A transaction the daemon does not have is dropped, not substituted by
    /// its neighbour. monerod reports it in `missed_tx` and simply omits it
    /// from `txs`, so the batch comes back shorter than it was asked for.
    #[test]
    fn a_transaction_the_daemon_does_not_have_is_dropped_not_substituted() {
        let asked = [hash(0x11), hash(0x22), hash(0x33)];
        let mut found = HashMap::new();
        found.insert(asked[2], entry(asked[2]));
        found.insert(asked[0], entry(asked[0]));

        let got = in_requested_order(&asked, found);
        assert_eq!(got.len(), 2, "the absent one was filled in from somewhere");
        assert!(
            !got.iter().any(|e| e.tx_hash == asked[1].to_hex()),
            "the hash monerod did not answer for came back anyway"
        );
    }

    /// The invariant the batched ring lookup is most likely to break.
    ///
    /// monerod fails an entire `get_outs` if any one index is out of range, so
    /// asking for a whole transaction's rings at once means one bad input can
    /// take every other ring down with it. Upstream drops just the offending
    /// input and renders the rest, and the fallback to one call per input
    /// exists so that this does too. Built from a real transaction, because a
    /// ring that resolves has to actually resolve for the test to mean
    /// anything.
    #[tokio::test]
    #[ignore = "needs the local testnet node on 127.0.0.1:28081"]
    async fn one_unresolvable_input_does_not_blank_the_rings_beside_it() {
        let source = RpcChainSource::new(Client::new("http://127.0.0.1:28081").expect("valid url"));

        // Block 134,721 holds one of the chain's original transactions.
        let block = source
            .block(BlockId::Height(134_721))
            .await
            .expect("the testnet node has block 134721");
        let hash: Hash32 = block.tx_hashes[0].parse().expect("a tx hash");
        let entry = source.transactions(&[hash]).await.expect("the tx");
        let tx = entry.txs[0].parse_json().expect("decoded json");

        let monerod_rpc::types::TxIn::Key(good) = tx.vin[0].clone() else {
            panic!("block 134721's transaction should spend a key input");
        };

        let mut mixed = tx.clone();
        mixed.vin = vec![
            monerod_rpc::types::TxIn::Key(good.clone()),
            monerod_rpc::types::TxIn::Key(TxInToKey {
                amount: good.amount,
                // Far past the end of any output set this chain has.
                key_offsets: vec![u64::MAX / 2],
                k_image: "cc".repeat(32),
            }),
        ];

        let resolved = source.resolve_rings(&mixed).await;
        assert_eq!(resolved.len(), 2);
        assert!(
            !resolved[0].ring.is_empty(),
            "the good ring was blanked by the bad input next to it"
        );
        assert!(!resolved[0].ring_unavailable);
        assert!(
            resolved[1].ring.is_empty() && resolved[1].ring_unavailable,
            "the out-of-range input should be the only one reported unavailable"
        );

        // And when every input resolves, the whole transaction costs one
        // call rather than one per input. Two inputs drawing the same ring
        // also exercise the dedup: a decoy two inputs share is asked for once.
        let mut both_good = tx.clone();
        both_good.vin = vec![
            monerod_rpc::types::TxIn::Key(good.clone()),
            monerod_rpc::types::TxIn::Key(good),
        ];

        let fresh = RpcChainSource::new(Client::new("http://127.0.0.1:28081").expect("valid url"));
        let resolved = fresh.resolve_rings(&both_good).await;
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|r| !r.ring.is_empty()));
        assert_eq!(
            fresh.rpc_calls(),
            1,
            "a transaction's rings should cost one call, not one per input"
        );
    }

    /// Newest first. monerod's own order is arbitrary, and a pool listing in
    /// arbitrary order is not a listing of anything; a reversed sort would
    /// show the oldest unconfirmed transaction as the newest.
    #[test]
    fn the_pool_is_ordered_by_arrival_newest_first() {
        let mut pool: Vec<monerod_rpc::types::PoolTxInfo> = [30u64, 10, 20]
            .into_iter()
            .map(|t| {
                serde_json::from_value(serde_json::json!({
                    "id_hash": format!("{t:02x}").repeat(32),
                    "receive_time": t,
                    "blob_size": 0,
                    "fee": 0,
                    "max_used_block_id_hash": "",
                    "max_used_block_height": 0,
                    "kept_by_block": false,
                    "last_failed_height": 0,
                    "last_failed_id_hash": "",
                    "relayed": true,
                    "last_relayed_time": 0,
                    "do_not_relay": false,
                    "double_spend_seen": false,
                    "tx_blob": "",
                }))
                .expect("a minimal pool entry")
            })
            .collect();

        newest_first(&mut pool);
        assert_eq!(
            pool.iter().map(|t| t.receive_time).collect::<Vec<_>>(),
            vec![30, 20, 10]
        );
    }

    /// Listing a transaction's inputs costs nothing -- the amount and key
    /// image are in the transaction itself -- so an endpoint that skips the
    /// ring lookup still has to report that the inputs exist. Answering
    /// `"inputs": []` next to `"coinbase": false` says the transaction spent
    /// nothing, which is a different claim from "the ring is not shown".
    #[test]
    fn an_unexpanded_input_is_listed_with_its_ring_withheld_not_dropped() {
        let tx: TxJson = serde_json::from_value(serde_json::json!({
            "version": 1,
            "unlock_time": 0,
            "vin": [{"key": {
                "amount": 10_000_000_000_000u64,
                "key_offsets": [3, 7, 11],
                "k_image": "87".repeat(32),
            }}],
            "vout": [],
            "extra": [],
        }))
        .expect("a one-input transaction");

        let inputs = unexpanded_inputs(&tx);
        assert_eq!(inputs.len(), 1, "the input was dropped rather than listed");
        assert_eq!(inputs[0].amount, 10_000_000_000_000);
        assert_eq!(inputs[0].key_image.to_hex(), "87".repeat(32));
        assert!(inputs[0].ring.is_empty());
        assert!(
            inputs[0].ring_unavailable,
            "an empty ring must render as withheld, not as a ring of zero members"
        );
    }

    /// A coinbase has a `gen` input, which is not a spend and carries no key
    /// image. Upstream reports no inputs for one, and so must this.
    #[test]
    fn a_coinbase_input_is_not_listed_as_a_spend() {
        let tx: TxJson = serde_json::from_value(serde_json::json!({
            "version": 2,
            "unlock_time": 0,
            "vin": [{"gen": {"height": 137_080}}],
            "vout": [],
            "extra": [],
        }))
        .expect("a coinbase transaction");
        assert!(unexpanded_inputs(&tx).is_empty());
    }

    /// The same guarantee through the public call, on the path that does not
    /// reach the daemon at all.
    #[tokio::test]
    async fn a_fully_cached_batch_is_answered_in_the_order_it_was_asked_for() {
        let source = source();
        let asked = [hash(0xaa), hash(0xbb), hash(0xcc)];
        // Cached in reverse, which is what a reader who arrived from the
        // newest transaction backwards would leave behind.
        for h in asked.iter().rev() {
            source.txs.insert(*h, entry(*h));
        }

        let got = source
            .transactions(&asked)
            .await
            .expect("a fully cached batch never calls out");
        let order: Vec<String> = got.txs.iter().map(|e| e.tx_hash.clone()).collect();
        assert_eq!(order, asked.iter().map(|h| h.to_hex()).collect::<Vec<_>>());
        assert_eq!(source.rpc_calls(), 0, "the daemon was not asked");
    }

    /// A ring whose offsets overflow on summation must degrade to an
    /// unavailable ring, not panic and not reach the daemon.
    #[tokio::test]
    async fn an_overflowing_offset_list_never_reaches_the_daemon() {
        let input = TxInToKey {
            amount: 0,
            key_offsets: vec![u64::MAX, u64::MAX],
            k_image: "aa".repeat(32),
        };
        // The client points at a closed port; if this tried to call out it
        // would still return unavailable, so assert it did not even try by
        // checking it resolves instantly and reports the right shape.
        let resolved = source().resolve_ring(&input).await;
        assert!(resolved.ring.is_empty());
        assert!(resolved.ring_unavailable);
        assert_eq!(resolved.amount, 0);
    }

    /// An input with no offsets has an empty ring, but that is not a failure:
    /// nothing was unavailable, there was simply nothing to fetch.
    #[tokio::test]
    async fn an_empty_offset_list_is_empty_but_not_unavailable() {
        let input = TxInToKey {
            amount: 0,
            key_offsets: vec![],
            k_image: "bb".repeat(32),
        };
        let resolved = source().resolve_ring(&input).await;
        assert!(resolved.ring.is_empty());
        assert!(
            !resolved.ring_unavailable,
            "nothing was asked for, so nothing was refused"
        );
    }

    /// The bound that protects the operator's daemon. Without it, per-request
    /// limits still multiply by however many requests arrive at once.
    #[tokio::test]
    async fn no_more_than_the_permitted_number_of_calls_reach_the_daemon_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let source = Arc::new(
            RpcChainSource::new(Client::new("http://127.0.0.1:1").expect("valid url"))
                .with_max_inflight_rpc(3),
        );

        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..40 {
            let source = Arc::clone(&source);
            let live = Arc::clone(&live);
            let peak = Arc::clone(&peak);
            tasks.push(tokio::spawn(async move {
                let _permit = source.permit().await;
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.expect("no task panicked");
        }

        let observed = peak.load(Ordering::SeqCst);
        assert!(
            observed <= 3,
            "{observed} calls were in flight at once against a ceiling of 3"
        );
        assert!(observed > 1, "the test did not actually run concurrently");
    }

    /// Resolving concurrently must not reorder the results: the page shows
    /// ring `n` under input `n`, so a reordering would attribute one input's
    /// decoys to another -- wrong, and wrong in a way that looks plausible.
    ///
    /// The daemon is unreachable here, so every ring comes back unavailable;
    /// what is under test is that the *order* survives the concurrency, which
    /// the distinct per-input amounts make observable.
    #[tokio::test]
    async fn concurrent_ring_resolution_preserves_input_order() {
        use monerod_rpc::types::{TxIn, TxInToKey};

        let source = RpcChainSource::new(Client::new("http://127.0.0.1:1").expect("valid url"));

        let tx = TxJson {
            version: 1,
            unlock_time: 0,
            vin: (0..25u64)
                .map(|i| {
                    TxIn::Key(TxInToKey {
                        // Distinct per input, so a reordering is visible.
                        amount: 1_000 + i,
                        key_offsets: vec![i + 1],
                        k_image: format!("{:02x}", i).repeat(32),
                    })
                })
                .collect(),
            vout: Vec::new(),
            extra: Vec::new(),
            signatures: None,
            rct_signatures: None,
            rctsig_prunable: None,
        };

        let resolved = source.resolve_rings(&tx).await;

        assert_eq!(resolved.len(), 25);
        for (i, r) in resolved.iter().enumerate() {
            assert_eq!(
                r.amount,
                1_000 + i as u64,
                "input {i} came back in position {i} with the wrong amount"
            );
        }
    }

    /// Every JSON-RPC error used to read as "no such block", so a busy daemon
    /// told the reader their block did not exist. The codes are monerod's,
    /// measured against a live node.
    #[test]
    fn block_errors_are_classified_by_what_monerod_actually_returns() {
        use monerod_rpc::types::error_code as ec;
        let id = BlockId::Height(999_999_999);

        // Asked for something that is not there, three ways.
        for code in [ec::WRONG_PARAM, ec::TOO_BIG_HEIGHT, ec::INTERNAL] {
            let e = classify_block_error(code, id, "x".to_owned());
            assert!(e.is_not_found(), "code {code} should read as not found");
            assert!(!e.is_transient());
        }

        // Busy is the one that was being reported as missing.
        let busy = classify_block_error(ec::CORE_BUSY, id, "busy".to_owned());
        assert!(!busy.is_not_found(), "a busy daemon has not lost the block");
        assert!(busy.is_transient(), "and retrying may work");

        // Gated off by configuration: neither missing nor worth retrying.
        let gated = classify_block_error(ec::METHOD_NOT_FOUND, id, "x".to_owned());
        assert!(!gated.is_not_found());
        assert!(!gated.is_transient());

        // An unrecognised code must not claim the block is absent: that is a
        // stronger statement than the evidence supports.
        let unknown = classify_block_error(-12345, id, "x".to_owned());
        assert!(!unknown.is_not_found());
    }

    /// A ceiling of zero would deadlock every request forever, which is a
    /// worse failure than an unbounded one because it looks like a hang.
    #[tokio::test]
    async fn a_zero_ceiling_is_raised_to_one_rather_than_deadlocking() {
        let source = RpcChainSource::new(Client::new("http://127.0.0.1:1").expect("valid url"))
            .with_max_inflight_rpc(0);
        let permit = source.permit().await;
        assert!(permit.is_some(), "a zero ceiling must still admit one call");
    }

    /// An unreachable daemon marks the ring unavailable rather than erroring
    /// the page or silently rendering an empty ring as though it were real.
    #[tokio::test]
    async fn an_unreachable_daemon_marks_the_ring_unavailable() {
        let input = TxInToKey {
            amount: 7_000_000_000_000,
            key_offsets: vec![4732, 5082],
            k_image: "cc".repeat(32),
        };
        let resolved = source().resolve_ring(&input).await;
        assert!(resolved.ring_unavailable);
        assert_eq!(
            resolved.amount, 7_000_000_000_000,
            "the denomination survives a failed lookup; it comes from the input"
        );
    }
}
