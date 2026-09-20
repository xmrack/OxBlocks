//! The HTML interface.
//!
//! Templates are compiled by askama, which escapes every interpolation by
//! default. That is the whole defence against the class of bug that made
//! upstream's 7,178-line `page.h` risky: there, markup was assembled by string
//! concatenation, so a missed escape was invisible. Here an unescaped value
//! requires writing `|safe`, which greps.
//!
//! No JavaScript, no cookies, no images, no external requests. The
//! Content-Security-Policy is `default-src 'none'; style-src 'self'`, so the
//! browser enforces that independently of what these templates emit.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use explorer_core::fmt::{age, now, remove_bad_chars, timestamp_utc};
use explorer_core::{Amount, BlockId, ChainError, Hash32, TxFacts};
use monerod_rpc::types::TxOutTarget;

use crate::api::handlers::{AppState, Shared};

/// The chain summary strip shown on every page.
pub struct ChainStatus {
    pub height: u64,
    pub nettype: String,
    pub difficulty: String,
    pub pool: u64,
    pub target: u64,
    pub syncing: bool,
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    version: &'static str,
    query: Option<String>,
    chain: Option<ChainStatus>,
    blocks: Vec<BlockRow>,
    page: u64,
}

struct BlockRow {
    height: u64,
    age: String,
    size: u64,
    tx_count: u64,
    fees: String,
    hash: String,
}

#[derive(Template)]
#[template(path = "block.html")]
struct BlockPage {
    version: &'static str,
    query: Option<String>,
    chain: Option<ChainStatus>,
    height: u64,
    depth: u64,
    hash: String,
    prev_hash: String,
    timestamp: u64,
    timestamp_utc: String,
    age: String,
    size: u64,
    weight: u64,
    tx_count: usize,
    reward: String,
    difficulty: String,
    nonce: u32,
    major_version: u8,
    minor_version: u8,
    txs: Vec<BlockTxRow>,
}

struct BlockTxRow {
    hash: String,
    coinbase: bool,
    outputs: usize,
    fee: String,
    ring: usize,
    size: u64,
}

#[derive(Template)]
#[template(path = "tx.html")]
struct TxPage {
    version: &'static str,
    query: Option<String>,
    chain: Option<ChainStatus>,
    hash: String,
    coinbase: bool,
    in_pool: bool,
    pruned: bool,
    block_height: u64,
    confirmations: u64,
    timestamp: u64,
    timestamp_utc: String,
    age: String,
    fee: String,
    size: u64,
    version_no: u64,
    rct_type: u8,
    ring_size: usize,
    unlock_time: u64,
    payment_id: String,
    payment_id8: String,
    inputs: Vec<InputView>,
    outputs: Vec<OutputView>,
    has_view_tags: bool,
    extra: String,
    extra_fields: Vec<ExtraField>,
    extra_undecoded: bool,
}

struct InputView {
    key_image: String,
    amount: String,
    unavailable: bool,
    ring: Vec<RingView>,
}

struct RingView {
    height: u64,
    public_key: String,
    tx_hash: String,
}

struct OutputView {
    public_key: String,
    amount: String,
    view_tag: String,
}

struct ExtraField {
    name: String,
    value: String,
}

#[derive(Template)]
#[template(path = "mempool.html")]
struct MempoolPage {
    version: &'static str,
    query: Option<String>,
    chain: Option<ChainStatus>,
    txs: Vec<PoolRow>,
}

struct PoolRow {
    hash: String,
    age: String,
    fee: String,
    ring: usize,
    size: u64,
}

#[derive(Template)]
#[template(path = "altblocks.html")]
struct AltBlocksPage {
    version: &'static str,
    query: Option<String>,
    chain: Option<ChainStatus>,
    chains: Vec<AltChainRow>,
}

struct AltChainRow {
    /// The chain's **first** block -- where it diverged -- not its tip.
    /// monerod names this `height`, which reads like a tip and is not one.
    height: u64,
    length: u64,
    tip: u64,
    block_hash: String,
    difficulty: String,
    parent: String,
}

/// The JSON API's own documentation.
///
/// Every limit on this page is interpolated from the constant the handler
/// actually enforces, and the accepted postfix lengths are computed by asking
/// the validator. Documentation that restates a number is documentation that
/// will one day be wrong; documentation that reads it cannot be.
#[derive(Template)]
#[template(path = "api.html")]
struct ApiPage {
    version: &'static str,
    query: Option<String>,
    chain: Option<ChainStatus>,
    /// Whether this daemon answered the `get_txids_loose` probe at startup.
    txids_loose: bool,
    /// Real heights, so the example links are clickable rather than
    /// illustrative.
    sample_height: u64,
    sample_range_start: u64,
    /// "5 characters", or "2 or 3 characters" on a smaller chain.
    postfix_lengths: String,
    max_transactions_limit: u64,
    max_mempool_limit: u64,
    max_block_range: u64,
    min_postfix_len: usize,
    max_postfix_len: usize,
    min_anonymity_set: u64,
    max_private_tx_matches: u64,
    recent_blocks: u64,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    version: &'static str,
    query: Option<String>,
    chain: Option<ChainStatus>,
    title: String,
    detail: String,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A rendered page, or a rendered explanation of why there is not one.
pub struct Page(StatusCode, String);

impl IntoResponse for Page {
    fn into_response(self) -> Response {
        (self.0, Html(self.1)).into_response()
    }
}

fn render<T: Template>(status: StatusCode, page: &T) -> Page {
    match page.render() {
        Ok(body) => Page(status, body),
        // A template failure is ours, not the caller's, and must not leak the
        // internals of why.
        Err(e) => {
            tracing::error!("template render failed: {e}");
            Page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "<h1>oxblocks</h1><p>Could not render this page.</p>".to_owned(),
            )
        }
    }
}

fn error_page(chain: Option<ChainStatus>, status: StatusCode, title: &str, detail: &str) -> Page {
    error_page_for(chain, status, title, detail, None)
}

/// As [`error_page`], but keeps the user's search term in the box so they can
/// correct it rather than retype it.
///
/// This is the one place arbitrary user text reaches a template. askama escapes
/// every interpolation, so it is safe by construction rather than by the caller
/// remembering -- `a_hostile_search_term_is_escaped_in_the_page` proves it.
fn error_page_for(
    chain: Option<ChainStatus>,
    status: StatusCode,
    title: &str,
    detail: &str,
    query: Option<String>,
) -> Page {
    render(
        status,
        &ErrorPage {
            version: VERSION,
            query,
            chain,
            title: title.to_owned(),
            detail: detail.to_owned(),
        },
    )
}

/// The status strip. Failure to fetch it must not fail the page around it, so
/// this returns `None` rather than an error.
async fn status_of(state: &AppState) -> Option<ChainStatus> {
    let info = state.chain.info().await.ok()?;
    Some(ChainStatus {
        height: info.height,
        nettype: info.nettype.clone(),
        difficulty: info.difficulty().to_string(),
        pool: info.tx_pool_size,
        target: info.target_height,
        syncing: info.target_height > info.height,
    })
}

fn xmr(atomic: u64) -> String {
    Amount::from_atomic(atomic).to_trimmed_xmr_string()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn index(state: Shared) -> Page {
    render_page(state, 0).await
}

/// Takes the page number as a string and parses it here.
///
/// Declaring it `Path<u64>` hands rejection to axum, which answers with its
/// own `text/plain` 400 quoting the input back -- the only route that did not
/// render this explorer's own error page. Harmless as an injection (the type
/// is `text/plain`, `nosniff` is set and the policy is `default-src 'none'`),
/// but it reflected raw input and read like a different piece of software.
pub async fn page_at(state: Shared, Path(raw): Path<String>) -> Page {
    let Ok(page) = remove_bad_chars(&raw).parse::<u64>() else {
        return error_page(
            status_of(&state).await,
            StatusCode::NOT_FOUND,
            "No such page",
            "A page number is a whole number, counting back from the newest block.",
        );
    };
    render_page(state, page).await
}

async fn render_page(State(state): Shared, page: u64) -> Page {
    const PER_PAGE: u64 = 25;

    let chain = status_of(&state).await;
    let Some(status) = chain.as_ref().map(|c| c.height) else {
        return error_page(
            None,
            StatusCode::BAD_GATEWAY,
            "monerod is unavailable",
            "The explorer could not reach its daemon. This is usually temporary.",
        );
    };

    let top = status.saturating_sub(1);
    let start = top.saturating_sub(page.saturating_mul(PER_PAGE));
    let end = start;
    let begin = start.saturating_sub(PER_PAGE.saturating_sub(1));

    let headers = match state.chain.headers_range(begin, end).await {
        Ok(h) => h.headers,
        Err(e) => {
            tracing::warn!("index: {e}");
            return error_page(
                chain,
                StatusCode::BAD_GATEWAY,
                "Could not load blocks",
                &e.public_message(),
            );
        }
    };

    let blocks = headers
        .iter()
        .rev()
        .map(|h| BlockRow {
            height: h.height,
            age: age(now(), h.timestamp),
            size: h.block_size,
            tx_count: h.num_txes,
            // The miner reward less the base emission is the fee total, but the
            // base is not exposed per block; show the reward, which is what a
            // reader can act on.
            fees: xmr(h.reward),
            hash: h.hash.to_lowercase(),
        })
        .collect();

    render(
        StatusCode::OK,
        &IndexPage {
            version: VERSION,
            query: None,
            chain,
            blocks,
            page,
        },
    )
}

pub async fn block(State(state): Shared, Path(raw): Path<String>) -> Page {
    let chain = status_of(&state).await;
    let cleaned = remove_bad_chars(&raw);

    let Ok(id) = BlockId::parse(&cleaned) else {
        return error_page(
            chain,
            StatusCode::NOT_FOUND,
            "No such block",
            &format!("{cleaned} is not a block height or a block hash."),
        );
    };

    let got = match state.chain.block(id).await {
        Ok(b) => b,
        Err(e) => return chain_error_page(chain, &e, &format!("No block {id}")),
    };

    let header = got.block_header.clone();
    let mut hashes: Vec<Hash32> = Vec::new();
    if let Ok(h) = header.miner_tx_hash.parse::<Hash32>() {
        hashes.push(h);
    }
    for h in &got.tx_hashes {
        if let Ok(h) = h.parse::<Hash32>() {
            hashes.push(h);
        }
    }

    let fetched = match state.chain.transactions(&hashes).await {
        Ok(f) => f,
        // Rendering the header on its own would show a block that holds
        // transactions as though it were empty -- a wrong page rather than a
        // partial one.
        Err(e) => return chain_error_page(chain, &e, "Could not load this block's transactions"),
    };
    let txs = fetched
        .txs
        .iter()
        .filter_map(|e| {
            let tx = e.parse_json().ok()?;
            let f = TxFacts::from_entry(e, &tx);
            Some(BlockTxRow {
                hash: e.tx_hash.to_lowercase(),
                coinbase: f.coinbase,
                outputs: tx.vout.len(),
                fee: xmr(f.fee),
                ring: f.ring_size,
                size: f.size,
            })
        })
        .collect::<Vec<_>>();

    render(
        StatusCode::OK,
        &BlockPage {
            version: VERSION,
            query: None,
            chain,
            height: header.height,
            depth: header.depth,
            hash: header.hash.to_lowercase(),
            prev_hash: header.prev_hash.to_lowercase(),
            timestamp: header.timestamp,
            timestamp_utc: timestamp_utc(header.timestamp),
            age: age(now(), header.timestamp),
            size: header.block_size,
            weight: header.block_weight,
            tx_count: txs.len(),
            reward: xmr(header.reward),
            difficulty: header.difficulty().to_string(),
            nonce: header.nonce,
            major_version: header.major_version,
            minor_version: header.minor_version,
            txs,
        },
    )
}

fn chain_error_page(chain: Option<ChainStatus>, e: &ChainError, title: &str) -> Page {
    if !e.is_not_found() {
        tracing::warn!("{title}: {e}");
    }
    if e.is_not_found() {
        error_page(chain, StatusCode::NOT_FOUND, title, &e.public_message())
    } else if e.is_transient() {
        error_page(
            chain,
            StatusCode::SERVICE_UNAVAILABLE,
            "monerod is busy",
            "The daemon is syncing or overloaded. This is usually temporary.",
        )
    } else {
        error_page(chain, StatusCode::BAD_GATEWAY, title, &e.public_message())
    }
}

pub async fn transaction(State(state): Shared, Path(raw): Path<String>) -> Page {
    let chain = status_of(&state).await;
    let cleaned = remove_bad_chars(&raw);

    let Ok(hash) = cleaned.parse::<Hash32>() else {
        return error_page(
            chain,
            StatusCode::NOT_FOUND,
            "No such transaction",
            &format!("{cleaned} is not a transaction hash."),
        );
    };

    let fetched = match state.chain.transactions(std::slice::from_ref(&hash)).await {
        Ok(f) => f,
        Err(e) => return chain_error_page(chain, &e, &format!("No transaction {hash}")),
    };

    let Some(entry) = fetched.txs.first() else {
        return error_page(
            chain,
            StatusCode::NOT_FOUND,
            "No such transaction",
            &format!("The daemon does not have transaction {hash}."),
        );
    };

    let Ok(tx) = entry.parse_json() else {
        return error_page(
            chain,
            StatusCode::BAD_GATEWAY,
            "Could not decode this transaction",
            "The daemon returned a transaction this explorer could not read.",
        );
    };

    let f = TxFacts::from_entry(entry, &tx);
    let rings = state.chain.resolve_rings(&tx).await;

    // A transaction in the pool is in no block, so the time it carries is the
    // time it arrived: `block_timestamp` is 0 there and renders as 1970. The
    // JSON API draws the same distinction in `Placement`.
    let when = if entry.in_pool {
        entry.received_timestamp
    } else {
        entry.block_timestamp
    };

    let inputs = rings
        .iter()
        .map(|r| InputView {
            key_image: r.key_image.to_hex(),
            amount: xmr(r.amount),
            unavailable: r.ring_unavailable,
            ring: r
                .ring
                .iter()
                .map(|m| RingView {
                    height: m.block_height,
                    public_key: m.public_key.to_hex(),
                    tx_hash: m.tx_hash.to_hex(),
                })
                .collect(),
        })
        .collect();

    let mut has_view_tags = false;
    let outputs = tx
        .vout
        .iter()
        .map(|o| {
            let (key, view_tag) = match &o.target {
                TxOutTarget::Key(k) => (k.clone(), String::new()),
                TxOutTarget::TaggedKey(t) => {
                    has_view_tags = true;
                    (t.key.clone(), t.view_tag.clone())
                }
                _ => (String::new(), String::new()),
            };
            OutputView {
                public_key: key,
                amount: if o.amount == 0 {
                    "0".to_owned()
                } else {
                    xmr(o.amount)
                },
                view_tag,
            }
        })
        .collect();

    let parsed = &f.extra;
    let mut extra_fields = Vec::new();
    if let Some(k) = parsed.tx_pub_key_explorer_compat() {
        extra_fields.push(ExtraField {
            name: "Transaction public key".to_owned(),
            value: k.to_hex(),
        });
    }
    for (i, k) in parsed.additional_pub_keys().iter().enumerate() {
        extra_fields.push(ExtraField {
            name: format!("Additional public key {}", i + 1),
            value: k.to_hex(),
        });
    }
    if let Some((depth, root)) = parsed.merge_mining_tag() {
        extra_fields.push(ExtraField {
            name: format!("Merge mining tag (depth {depth})"),
            value: root.to_hex(),
        });
    }

    render(
        StatusCode::OK,
        &TxPage {
            version: VERSION,
            query: None,
            chain,
            hash: entry.tx_hash.to_lowercase(),
            coinbase: f.coinbase,
            in_pool: entry.in_pool,
            pruned: entry.prunable_missing(&tx),
            block_height: entry.block_height,
            confirmations: entry.confirmations,
            timestamp: when,
            timestamp_utc: timestamp_utc(when),
            // The clock, not an extrapolation from the confirmation count: at
            // a flat 120 seconds a block this reported a block mined seconds
            // ago as two minutes old, and disagreed with the block page beside
            // it, which has always used the clock.
            age: age(now(), when),
            fee: xmr(f.fee),
            size: f.size,
            version_no: f.version,
            rct_type: f.rct_type,
            ring_size: f.ring_size,
            unlock_time: f.unlock_time,
            payment_id: f.payment_id_hex(),
            payment_id8: f.payment_id8_hex(),
            inputs,
            outputs,
            has_view_tags,
            extra: f.extra_hex(),
            extra_fields,
            extra_undecoded: !parsed.is_complete(),
        },
    )
}

pub async fn mempool(State(state): Shared) -> Page {
    let chain = status_of(&state).await;

    let pool = match state.chain.mempool().await {
        Ok(p) => p,
        Err(ChainError::NeedsUnrestricted(what)) => {
            return error_page(
                chain,
                StatusCode::NOT_IMPLEMENTED,
                "The mempool is unavailable",
                &format!(
                    "Showing {what} needs an unrestricted daemon. This one runs with \
                     --restricted-rpc, which blocks /get_transaction_pool."
                ),
            );
        }
        Err(e) => return chain_error_page(chain, &e, "Could not load the mempool"),
    };

    // Measured against the clock rather than against the newest entry in the
    // pool: the latter always shows the newest transaction as having waited no
    // time at all, even on a pool nothing has arrived in for an hour.
    let asked_at = now();

    let txs = pool
        .transactions
        .iter()
        .filter_map(|t| {
            let tx = t.parse_json().ok()?;
            let f = TxFacts::from_pool(t, &tx);
            Some(PoolRow {
                hash: t.id_hash.to_lowercase(),
                age: age(asked_at, t.receive_time),
                fee: xmr(f.fee),
                ring: f.ring_size,
                size: f.size,
            })
        })
        .collect();

    render(
        StatusCode::OK,
        &MempoolPage {
            version: VERSION,
            query: None,
            chain,
            txs,
        },
    )
}

pub async fn alt_blocks(State(state): Shared) -> Page {
    let chain = status_of(&state).await;

    let alt = match state.chain.alt_chains().await {
        Ok(a) => a,
        Err(ChainError::NeedsUnrestricted(what)) => {
            return error_page(
                chain,
                StatusCode::NOT_IMPLEMENTED,
                "Alternative chains are unavailable",
                &format!(
                    "Showing {what} needs an unrestricted daemon. This one runs with \
                     --restricted-rpc, which blocks get_alternate_chains."
                ),
            );
        }
        Err(e) => return chain_error_page(chain, &e, "Could not load alternative chains"),
    };

    let chains = alt
        .chains
        .iter()
        .map(|c| AltChainRow {
            height: c.height,
            length: c.length,
            tip: c.height.saturating_add(c.length).saturating_sub(1),
            block_hash: c.block_hash.to_lowercase(),
            // ChainInfo has no accessor of its own; reassembling here keeps
            // one call site rather than adding a fourth place a 128-bit
            // value could be read from the wrong top word.
            difficulty: monerod_rpc::types::reassemble_u128(c.difficulty, c.difficulty_top64)
                .to_string(),
            parent: c.main_chain_parent_block.to_lowercase(),
        })
        .collect();

    render(
        StatusCode::OK,
        &AltBlocksPage {
            version: VERSION,
            query: None,
            chain,
            chains,
        },
    )
}

#[derive(serde::Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
}

/// Search dispatches on shape and then *redirects*, so the address bar ends up
/// on the canonical page rather than on a query string.
pub async fn search(State(state): Shared, Query(q): Query<SearchQuery>) -> Response {
    let raw = q.q.unwrap_or_default();
    let cleaned = remove_bad_chars(raw.trim());

    if cleaned.is_empty() {
        return axum::response::Redirect::to("/").into_response();
    }

    match BlockId::parse(&cleaned) {
        Ok(BlockId::Height(_)) => {
            return axum::response::Redirect::to(&format!("/block/{cleaned}")).into_response();
        }
        Ok(BlockId::Hash(hash)) => {
            // A block hash and a transaction hash are the same shape, so the
            // only way to tell them apart is to look one up.
            let target = if state.chain.block(BlockId::Hash(hash)).await.is_ok() {
                "block"
            } else {
                "tx"
            };
            return axum::response::Redirect::to(&format!("/{target}/{cleaned}")).into_response();
        }
        Err(_) => {}
    }

    let chain = status_of(&state).await;
    error_page_for(
        chain,
        StatusCode::NOT_FOUND,
        "Nothing found",
        "Enter a block height, a block hash, or a transaction hash. Monero has \
         no address index, so addresses cannot be searched.",
        Some(raw),
    )
    .into_response()
}

/// The stylesheet, compiled into the binary.
///
/// Embedded rather than read from disk so the binary is self-contained: there
/// is no asset directory to deploy, and no path for a misconfigured server to
/// expose.
pub async fn stylesheet() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../static/style.css"),
    )
        .into_response()
}

/// Render a list of accepted lengths as English: "5 characters",
/// "2 or 3 characters", "2, 3 or 4 characters".
fn describe_lengths(lengths: &[usize]) -> String {
    let names: Vec<String> = lengths.iter().map(ToString::to_string).collect();
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => format!("{last} characters"),
        Some((last, rest)) => format!("{} or {last} characters", rest.join(", ")),
    }
}

/// The API documentation, which is what the `API` link in the header points at.
///
/// It used to point at `/api/networkinfo`, which answered a reader looking for
/// documentation with a wall of raw JSON.
pub async fn api_docs(State(state): Shared) -> Page {
    let chain = status_of(&state).await;

    // Already fetched by `status_of` a moment ago, so this is a cache hit
    // rather than a second round trip.
    let info = state.chain.info().await.ok();
    let height = info.as_ref().map_or(0, |i| i.height);
    let sample_height = height.saturating_sub(1);

    let lengths = info
        .as_ref()
        .map(|i| crate::api::handlers::acceptable_postfix_lengths(i))
        .unwrap_or_default();

    render(
        StatusCode::OK,
        &ApiPage {
            version: VERSION,
            query: None,
            chain,
            txids_loose: state.txids_loose.load(std::sync::atomic::Ordering::Relaxed),
            sample_height,
            sample_range_start: sample_height.saturating_sub(9),
            postfix_lengths: describe_lengths(&lengths),
            max_transactions_limit: crate::api::handlers::MAX_TRANSACTIONS_LIMIT,
            max_mempool_limit: crate::api::handlers::MAX_MEMPOOL_LIMIT,
            max_block_range: crate::api::handlers::MAX_BLOCK_RANGE,
            min_postfix_len: crate::api::handlers::MIN_POSTFIX_LEN,
            max_postfix_len: crate::api::handlers::MAX_POSTFIX_LEN,
            min_anonymity_set: crate::api::handlers::MIN_ANONYMITY_SET,
            max_private_tx_matches: crate::api::handlers::MAX_PRIVATE_TX_MATCHES,
            recent_blocks: crate::api::handlers::RECENT_BLOCKS,
        },
    )
}

pub async fn not_found(State(state): Shared) -> Page {
    let chain = status_of(&state).await;
    error_page(
        chain,
        StatusCode::NOT_FOUND,
        "Page not found",
        "There is no such page.",
    )
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

    fn status() -> Option<ChainStatus> {
        Some(ChainStatus {
            height: 3_185_431,
            nettype: "mainnet".to_owned(),
            difficulty: "691253322598".to_owned(),
            pool: 7,
            target: 0,
            syncing: false,
        })
    }

    /// The defence the whole HTML layer rests on. Upstream builds markup by
    /// string concatenation across 7,178 lines, where a missed escape is
    /// invisible; here escaping is the default and opting out requires `|safe`.
    #[test]
    fn a_hostile_search_term_is_escaped_in_the_page() {
        let page = ErrorPage {
            version: VERSION,
            query: Some(r#"<script>alert(1)</script>"#.to_owned()),
            chain: status(),
            title: "Nothing found".to_owned(),
            detail: r#"also "quoted" & <dangerous>"#.to_owned(),
        };
        let html = page.render().expect("renders");

        assert!(
            !html.contains("<script>"),
            "a raw script tag reached the page"
        );
        assert!(!html.contains("alert(1)</script>"));
        assert!(html.contains("&#60;script&#62;"), "escaped form is present");
        // The detail text is escaped too, including the quote that would
        // otherwise break out of an attribute.
        assert!(!html.contains(r#""quoted""#));
    }

    /// No script element, no inline handler, no external origin: the page must
    /// satisfy `default-src 'none'; style-src 'self'` on its own, not merely be
    /// protected by the header.
    #[test]
    fn rendered_pages_contain_no_script_and_no_external_reference() {
        let page = ErrorPage {
            version: VERSION,
            query: None,
            chain: status(),
            title: "Page not found".to_owned(),
            detail: "There is no such page.".to_owned(),
        };
        let html = page.render().expect("renders").to_lowercase();

        assert!(!html.contains("<script"));
        assert!(!html.contains("javascript:"));
        assert!(!html.contains(" onclick"));
        assert!(!html.contains(" onload"));
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("<img"));
        // The only asset is our own stylesheet, served from this binary.
        assert_eq!(html.matches("/static/style.css").count(), 1);
    }

    #[test]
    fn the_chain_strip_renders_its_values() {
        let page = ErrorPage {
            version: VERSION,
            query: None,
            chain: status(),
            title: "t".to_owned(),
            detail: "d".to_owned(),
        };
        let html = page.render().expect("renders");
        assert!(html.contains("3185431"));
        assert!(html.contains("mainnet"));
        assert!(html.contains("691253322598"));
    }

    fn api_page() -> ApiPage {
        use crate::api::handlers as h;
        ApiPage {
            version: VERSION,
            query: None,
            chain: status(),
            txids_loose: true,
            sample_height: 3_185_430,
            sample_range_start: 3_185_421,
            postfix_lengths: describe_lengths(&[5]),
            max_transactions_limit: h::MAX_TRANSACTIONS_LIMIT,
            max_mempool_limit: h::MAX_MEMPOOL_LIMIT,
            max_block_range: h::MAX_BLOCK_RANGE,
            min_postfix_len: h::MIN_POSTFIX_LEN,
            max_postfix_len: h::MAX_POSTFIX_LEN,
            min_anonymity_set: h::MIN_ANONYMITY_SET,
            max_private_tx_matches: h::MAX_PRIVATE_TX_MATCHES,
            recent_blocks: h::RECENT_BLOCKS,
        }
    }

    /// Every `/api/*` route the router registers must be documented.
    ///
    /// Read out of `main.rs`'s own source, so adding a route and forgetting to
    /// write it up fails the build rather than shipping a documentation page
    /// that quietly omits an endpoint. Compared on the part of the pattern
    /// before its first `{placeholder}`, because the page spells arguments
    /// `<hash>` where axum spells them `{hash}`.
    #[test]
    fn every_api_route_appears_in_the_documentation() {
        let router_source = include_str!("main.rs");
        let page = api_page().render().expect("renders");

        let mut routes: Vec<&str> = Vec::new();
        for (at, _) in router_source.match_indices("\"/api") {
            let rest = router_source.get(at + 1..).unwrap_or_default();
            if let Some(end) = rest.find('"')
                && let Some(pattern) = rest.get(..end)
            {
                routes.push(pattern);
            }
        }
        routes.sort_unstable();
        routes.dedup();

        assert!(
            routes.len() >= 13,
            "only found {} routes in the router source, so this test is not \
             actually reading the route table: {routes:?}",
            routes.len()
        );

        for route in routes {
            let stem = route.split('{').next().unwrap_or(route);
            assert!(
                page.contains(stem),
                "{route} is routed but {stem} appears nowhere in the API \
                 documentation page"
            );
        }
    }

    /// The limits on the page are the constants the handlers enforce, not
    /// numbers typed into the template beside them. Changing a cap without
    /// touching the template fails here.
    #[test]
    fn the_documented_limits_are_the_enforced_ones() {
        use crate::api::handlers as h;
        let page = api_page().render().expect("renders");

        for value in [
            h::MAX_TRANSACTIONS_LIMIT,
            h::MAX_MEMPOOL_LIMIT,
            h::MAX_BLOCK_RANGE,
            h::RECENT_BLOCKS,
        ] {
            assert!(
                page.contains(&format!("<td class=\"num\">{value}</td>")),
                "{value} is enforced but the limits table does not list it"
            );
        }

        // The anonymity band is one cell holding both ends of it.
        assert!(
            page.contains(&format!(
                "<td class=\"num\">{}&ndash;{}</td>",
                h::MIN_ANONYMITY_SET,
                h::MAX_PRIVATE_TX_MATCHES
            )),
            "the limits table does not state the accepted anonymity band"
        );
    }

    /// The documentation page is subject to the same rule as every other page:
    /// nothing external, no script, one stylesheet. Example requests are shown
    /// as paths rather than absolute URLs partly for this reason -- a page read
    /// over Tor should not carry a hostname somebody might click.
    #[test]
    fn the_api_page_references_nothing_external() {
        let html = api_page().render().expect("renders").to_lowercase();
        assert!(!html.contains("<script"));
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("<img"));
        assert_eq!(html.matches("/static/style.css").count(), 1);
    }

    /// The daemon-specific notice: a deployment whose daemon cannot serve the
    /// k-anonymous lookup must say so on the page rather than documenting an
    /// endpoint that will refuse.
    #[test]
    fn the_page_reports_whether_this_daemon_serves_the_private_lookup() {
        let available = api_page().render().expect("renders");
        assert!(available.contains("available"));
        assert!(!available.contains("Unavailable on this deployment"));

        let mut page = api_page();
        page.txids_loose = false;
        let missing = page.render().expect("renders");
        assert!(missing.contains("Unavailable on this deployment"));
        assert!(missing.contains("get_txids_loose"));
    }

    #[test]
    fn accepted_postfix_lengths_read_as_english() {
        assert_eq!(describe_lengths(&[]), "");
        assert_eq!(describe_lengths(&[5]), "5 characters");
        assert_eq!(describe_lengths(&[2, 3]), "2 or 3 characters");
        assert_eq!(describe_lengths(&[2, 3, 4]), "2, 3 or 4 characters");
    }

    /// A page must still render when the daemon could not be reached, because
    /// that is exactly when someone is looking at it.
    #[test]
    fn a_page_renders_without_a_chain_status() {
        let page = ErrorPage {
            version: VERSION,
            query: None,
            chain: None,
            title: "monerod is unavailable".to_owned(),
            detail: "could not reach the daemon".to_owned(),
        };
        let html = page.render().expect("renders");
        assert!(html.contains("monerod is unavailable"));
        assert!(!html.contains("class=\"status\""));
    }
}
