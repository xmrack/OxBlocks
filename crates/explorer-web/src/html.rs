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
    /// Every transaction in the block, coinbase included, matching the block
    /// page's own count and the number of rows in its table. `num_txes`
    /// counts non-coinbase transactions only; see `total_tx_count`.
    tx_count: u64,
    reward: String,
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
    /// Every transaction rendered in the table below, coinbase included.
    /// Agrees with the index row's `total_tx_count(num_txes)` because every
    /// valid block carries exactly one coinbase.
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
    pool_payout: bool,
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
    pool_payout: bool,
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
    amount: Option<String>,
    unavailable: bool,
    ring: Vec<RingView>,
    ages: AgeStrip,
}

struct RingView {
    height: u64,
    public_key: String,
    tx_hash: String,
}

/// An input's ring laid out along a time axis.
struct AgeStrip {
    marks: Vec<AgeMark>,
    ticks: Vec<AgeTick>,
}

/// One ring member's place on the strip: a hairline at its own age, under a
/// wide translucent halo. Halos of members close in age overlap into a darker
/// band, which is what makes the clustering visible at a glance.
struct AgeMark {
    /// Left edges, in the strip's own coordinate space. Computed here because
    /// the Content-Security-Policy forbids inline styles, so the SVG carries
    /// presentation attributes rather than a `style=`.
    halo: u32,
    stem: u32,
    /// The age this mark stands for, e.g. "4 h".
    label: String,
}

/// One labelled point on the strip's time axis.
struct AgeTick {
    x: u32,
    label: String,
}

/// Whether a coinbase pays many recipients at once.
///
/// A solo miner or a custodial pool takes the reward to one output and
/// distributes off-chain, so its coinbase has exactly one. A decentralised
/// pool pays every participant in the coinbase itself, which is why p2pool
/// blocks carry dozens. That shape is visible from chain data alone and costs
/// nothing to check, but it identifies the *shape*, not the software: this
/// says "paid many recipients", not "was mined by p2pool".
///
/// Attributing a payout to a particular pool, or telling which ring member is
/// a payout being swept, needs that pool's own sidechain records. None of it
/// is on the Monero chain, so this explorer cannot and does not infer it.
fn is_pool_payout(coinbase: bool, outputs: usize) -> bool {
    coinbase && outputs > 1
}

/// Blocks per hour and per day at Monero's two-minute target.
const BLOCKS_PER_HOUR: u64 = 30;
const BLOCKS_PER_DAY: u64 = 720;

/// The strip's coordinate space, in the pixels it occupies at full size. The
/// stylesheet lets it shrink with a narrow window but never enlarges it, so
/// the axis labels stay the size they were drawn at.
const STRIP_WIDTH: u32 = 760;
const STRIP_BAND: u32 = 26;
const STRIP_HEIGHT: u32 = 44;
const HALO_WIDTH: u32 = 26;
const STEM_WIDTH: u32 = 2;
/// Baseline for the axis labels: below the band, with room for descenders.
const TICK_BASELINE: u32 = STRIP_HEIGHT - 5;

/// Axis labels, spaced widely enough on a log scale that two never collide.
const AGE_TICKS: [(u64, &str); 7] = [
    (BLOCKS_PER_HOUR, "1h"),
    (6 * BLOCKS_PER_HOUR, "6h"),
    (BLOCKS_PER_DAY, "1d"),
    (7 * BLOCKS_PER_DAY, "1w"),
    (30 * BLOCKS_PER_DAY, "1mo"),
    (365 * BLOCKS_PER_DAY, "1y"),
    (1825 * BLOCKS_PER_DAY, "5y"),
];

/// The axis every strip on a transaction shares: the oldest age any of its
/// inputs reaches. Drawn to its own scale, each input would put the same
/// cluster in a different place, and the strips could not be compared.
fn axis_span(heights: impl Iterator<Item = u64>, spent_at: u64) -> u64 {
    heights
        .map(|h| spent_at.saturating_sub(h))
        .max()
        .unwrap_or(0)
}

/// Lays a ring out on a time axis running from `oldest` at the left to the
/// spend itself at the right.
///
/// `spent_at` is the height of the block holding the spending transaction, or
/// the current tip for one still in the pool. A member mined *after* that --
/// which the daemon should never return -- reads as brand new rather than
/// wrapping.
///
/// `oldest` is the axis, in blocks, and is the oldest age reached by any of
/// the transaction's inputs rather than by this one alone.
fn age_strip(ring: &[RingView], spent_at: u64, oldest: u64) -> AgeStrip {
    let ages = ring.iter().map(|m| spent_at.saturating_sub(m.height));

    AgeStrip {
        marks: ages
            .map(|age| {
                let x = strip_x(age, oldest);
                AgeMark {
                    halo: centred(x, HALO_WIDTH),
                    stem: centred(x, STEM_WIDTH),
                    label: age_label(age),
                }
            })
            .collect(),
        // Only the range the ring covers: a label with nothing under it
        // invites the reader to look for members that are not there.
        ticks: AGE_TICKS
            .iter()
            .filter(|&&(age, _)| age <= oldest)
            .map(|&(age, label)| AgeTick {
                x: strip_x(age, oldest),
                label: label.to_owned(),
            })
            .collect(),
    }
}

/// Left edge of a mark of `width` centred on `x`, held inside the strip so a
/// member at either extreme is drawn whole rather than half outside the band.
fn centred(x: u32, width: u32) -> u32 {
    x.saturating_sub(width / 2).min(STRIP_WIDTH - width)
}

/// Where an age sits along the strip: 0 is the oldest member, `STRIP_WIDTH`
/// the moment of the spend.
///
/// Logarithmic, because decoys are drawn from a gamma distribution that
/// strongly favours recent outputs. On a linear axis nearly every member of a
/// healthy ring lands within a unit or two of the right edge, and the shape
/// worth looking at is the one that disappears.
fn strip_x(age: u64, oldest: u64) -> u32 {
    #[allow(
        clippy::cast_precision_loss,
        reason = "a chart coordinate, not chain arithmetic"
    )]
    let (age, oldest) = (age as f64, oldest as f64);

    let span = (oldest + 1.0).ln();
    if span <= 0.0 {
        // No member is older than the spend, so there is no axis to spread
        // them along; they all belong at the spend end.
        return STRIP_WIDTH;
    }

    let from_left = f64::from(STRIP_WIDTH) * (1.0 - (age + 1.0).ln() / span);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to the strip before the cast"
    )]
    let x = from_left.clamp(0.0, f64::from(STRIP_WIDTH)).round() as u32;
    x
}

/// An age written the way a reader would say it.
fn age_label(blocks: u64) -> String {
    let minutes = blocks.saturating_mul(2);
    match minutes {
        0..60 => format!("{minutes} min"),
        60..1440 => format!("{} h", minutes / 60),
        1440..43200 => format!("{} d", minutes / 1440),
        _ => format!("{} mo", minutes / 43200),
    }
}

struct OutputView {
    public_key: String,
    amount: Option<String>,
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
    waiting_sort: ColumnSort,
    fee_sort: ColumnSort,
    size_sort: ColumnSort,
    txs: Vec<PoolRow>,
}

struct PoolRow {
    hash: String,
    age: String,
    waiting_secs: u64,
    fee: String,
    fee_atomic: u64,
    ring: usize,
    size: u64,
}

/// Which mempool column a page was sorted by, if any.
///
/// Only the columns backed by a plain number are sortable. `Hash` has no
/// useful order and `Ring` was not asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Waiting,
    Fee,
    Size,
}

impl SortKey {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "waiting" => Some(Self::Waiting),
            "fee" => Some(Self::Fee),
            "size" => Some(Self::Size),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Fee => "fee",
            Self::Size => "size",
        }
    }

    fn of(self, row: &PoolRow) -> u64 {
        match self {
            Self::Waiting => row.waiting_secs,
            Self::Fee => row.fee_atomic,
            Self::Size => row.size,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "asc" => Some(Self::Asc),
            "desc" => Some(Self::Desc),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    fn flipped(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

/// A column header's link: where clicking it goes, and whether an arrow
/// shows it is the column currently in effect.
///
/// No JavaScript runs on this page, so "clicking a header to sort" has to be
/// an ordinary link to a URL that already carries the answer.
struct ColumnSort {
    href: String,
    arrow: &'static str,
}

/// The header link for `key`, given the sort currently in effect (if any).
///
/// A column not currently sorted links to itself descending, largest or
/// longest-waiting first, which is normally the more interesting read. The
/// active column instead links to its own reverse, so a second click flips
/// it, and carries an arrow showing which way it is sorted now.
fn column_sort(key: SortKey, active: Option<(SortKey, SortDir)>) -> ColumnSort {
    let dir = match active {
        Some((k, d)) if k == key => d.flipped(),
        _ => SortDir::Desc,
    };
    let arrow = match active {
        Some((k, SortDir::Asc)) if k == key => " \u{25b2}",
        Some((k, SortDir::Desc)) if k == key => " \u{25bc}",
        _ => "",
    };
    ColumnSort {
        href: format!("/mempool?sort={}&dir={}", key.as_str(), dir.as_str()),
        arrow,
    }
}

/// How long a pool transaction has been waiting, in seconds.
fn waiting_secs(asked_at: u64, receive_time: u64) -> u64 {
    asked_at.abs_diff(receive_time)
}

/// Orders rows by `key`, stably: rows equal under `key` keep the order the
/// daemon returned them in, whichever direction is asked for.
fn sort_pool_rows(rows: &mut [PoolRow], key: SortKey, dir: SortDir) {
    rows.sort_by(|a, b| {
        let ord = key.of(a).cmp(&key.of(b));
        if dir == SortDir::Desc {
            ord.reverse()
        } else {
            ord
        }
    });
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

/// The stylesheet's bytes, embedded at compile time.
const STYLESHEET: &str = include_str!("../static/style.css");

/// A cache key derived from the stylesheet's own contents.
///
/// The stylesheet is served with a day-long `max-age` and carries no `ETag`,
/// so a returning browser reuses whatever it already has. At a fixed URL that
/// means a CSS change is invisible for a day: the page renders new markup
/// against an old stylesheet, which is how a `<details>` hint came out as a
/// bare disclosure triangle and a black blob. Changing the *URL* whenever the
/// bytes change makes the long cache lifetime correct instead of harmful.
///
/// FNV-1a, and deliberately not a cryptographic hash: this is a cache key, not
/// a signature, and nothing is trusted on the strength of it.
pub const STYLESHEET_VERSION: u64 = fnv1a(STYLESHEET.as_bytes());

const fn fnv1a(mut bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    // Walked by slice pattern rather than by index: the workspace denies
    // `indexing_slicing`, and this needs no bounds check to begin with.
    while let [first, rest @ ..] = bytes {
        hash ^= *first as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        bytes = rest;
    }
    hash
}

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

/// XMR with every decimal place shown.
///
/// `xmr` trims for a value read on its own; a table column is read against
/// its neighbours, and a fee of `0.6` above one of `0.00003064` does not
/// align on the decimal point unless both carry the same number of places.
fn xmr_aligned(atomic: u64) -> String {
    Amount::from_atomic(atomic).to_xmr_string()
}

/// The XMR value of an input or output, when there is one to show.
///
/// A RingCT amount is committed, not published: the cleartext field is zero
/// and means "hidden", not "nothing". `None` is that state, so the templates
/// cannot render it as a number.
///
/// This was a string comparison against `"0"` in two places. The output path
/// built that sentinel and matched; the input path formatted through `xmr`,
/// which yields `"0.0"`, so its guard never fired and every RingCT input was
/// labelled `0.0 XMR`. One function, no sentinel, no second copy to drift.
fn visible_amount(atomic: u64) -> Option<String> {
    (atomic != 0).then(|| xmr(atomic))
}

/// The index shows this so it agrees with the block page's own count.
///
/// `num_txes` is upstream's count of non-coinbase transactions -- see
/// `BlockHeader::num_txes` -- and every valid block carries exactly one
/// coinbase besides those, a consensus rule this crate does not itself
/// enforce but can rely on. The block page counts every row it renders
/// instead of applying this arithmetic a second time, so the two derivations
/// cannot silently drift apart the way `num_txes` alone once did: the index
/// showed 16, the block page (which folded the coinbase in) showed 17.
fn total_tx_count(num_txes: u64) -> u64 {
    num_txes.saturating_add(1)
}

/// The block page's half of the same total: every row it renders, coinbase
/// included. Named so a test can call the block handler's own arithmetic
/// directly, rather than recomputing `txs.len()` a second time and only
/// proving the two copies agree with each other.
fn table_tx_count(txs: &[BlockTxRow]) -> usize {
    txs.len()
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
            tx_count: total_tx_count(h.num_txes),
            // The reward, not the fee total. Fees are the reward less the base
            // emission, and no header field carries the base, so this column
            // cannot be a fee column without inventing the number. It was
            // headed "Fees" and showed this value, which read as a plausible
            // fee and was not one.
            reward: xmr_aligned(h.reward),
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
                pool_payout: is_pool_payout(f.coinbase, tx.vout.len()),
                outputs: tx.vout.len(),
                fee: xmr_aligned(f.fee),
                ring: f.ring_size,
                size: f.size,
            })
        })
        .collect::<Vec<_>>();
    let tx_count = table_tx_count(&txs);

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
            tx_count,
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

    // Ring ages are measured against the block that spent them, or the tip
    // for a transaction still in the pool.
    let spent_at = if entry.in_pool {
        chain.as_ref().map_or(0, |c| c.height)
    } else {
        entry.block_height
    };

    let oldest = axis_span(
        rings.iter().flat_map(|r| &r.ring).map(|m| m.block_height),
        spent_at,
    );

    let inputs: Vec<InputView> = rings
        .iter()
        .map(|r| {
            let ring: Vec<RingView> = r
                .ring
                .iter()
                .map(|m| RingView {
                    height: m.block_height,
                    public_key: m.public_key.to_hex(),
                    tx_hash: m.tx_hash.to_hex(),
                })
                .collect();
            InputView {
                key_image: r.key_image.to_hex(),
                amount: visible_amount(r.amount),
                unavailable: r.ring_unavailable,
                ages: age_strip(&ring, spent_at, oldest),
                ring,
            }
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
                amount: visible_amount(o.amount),
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
            pool_payout: is_pool_payout(f.coinbase, tx.vout.len()),
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

#[derive(serde::Deserialize)]
pub struct MempoolQuery {
    sort: Option<String>,
    dir: Option<String>,
}

/// The sort a `/mempool` request asked for, from its query string.
///
/// An unrecognised `sort` or `dir` -- a stale link, a typo -- is treated as
/// unsorted rather than failing the page. A `dir` with no `sort` names
/// nothing to reverse and is ignored.
fn active_sort(sort: Option<&str>, dir: Option<&str>) -> Option<(SortKey, SortDir)> {
    let key = SortKey::parse(sort?)?;
    Some((key, dir.and_then(SortDir::parse).unwrap_or(SortDir::Desc)))
}

pub async fn mempool(State(state): Shared, Query(q): Query<MempoolQuery>) -> Page {
    let chain = status_of(&state).await;
    let active = active_sort(q.sort.as_deref(), q.dir.as_deref());

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

    let mut txs: Vec<PoolRow> = pool
        .transactions
        .iter()
        .filter_map(|t| {
            let tx = t.parse_json().ok()?;
            let f = TxFacts::from_pool(t, &tx);
            Some(PoolRow {
                hash: t.id_hash.to_lowercase(),
                age: age(asked_at, t.receive_time),
                waiting_secs: waiting_secs(asked_at, t.receive_time),
                fee: xmr_aligned(f.fee),
                fee_atomic: f.fee,
                ring: f.ring_size,
                size: f.size,
            })
        })
        .collect();
    if let Some((key, dir)) = active {
        sort_pool_rows(&mut txs, key, dir);
    }

    render(
        StatusCode::OK,
        &MempoolPage {
            version: VERSION,
            query: None,
            chain,
            waiting_sort: column_sort(SortKey::Waiting, active),
            fee_sort: column_sort(SortKey::Fee, active),
            size_sort: column_sort(SortKey::Size, active),
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
        STYLESHEET,
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

    /// Every documented endpoint carries a runnable example, and the example
    /// points at the endpoint it is filed under.
    ///
    /// The second half is the part worth checking: a copied section whose
    /// `curl` line still names the endpoint above it is the likely mistake,
    /// and it reads as correct.
    #[test]
    fn every_documented_endpoint_shows_a_curl_example_for_itself() {
        let page = api_page().render().expect("renders");
        let mut checked = 0;

        for (at, _) in page.match_indices(r#"<section class="endpoint" id=""#) {
            let rest = page.get(at..).unwrap_or_default();
            let Some(body_end) = rest.find("</section>") else {
                panic!("unterminated endpoint section")
            };
            let section = rest.get(..body_end).unwrap_or_default();

            // The shared response shapes are not endpoints and take no request.
            let id_at = r#"<section class="endpoint" id=""#.len();
            let id = section
                .get(id_at..)
                .and_then(|r| r.split('"').next())
                .unwrap_or_default();
            if id.starts_with("shape-") {
                continue;
            }

            // The route as the heading states it, up to its first argument.
            let route = section
                .split_once(r#"<h3>"#)
                .and_then(|(_, r)| r.split_once("</h3>"))
                .and_then(|(h, _)| h.rsplit_once(r#"<code class="lit">"#))
                .and_then(|(_, c)| c.split_once("</code>"))
                .map(|(path, _)| path)
                .unwrap_or_default();
            let stem = route
                .split(['?'])
                .next()
                .unwrap_or_default()
                .split("&lt;")
                .next()
                .unwrap_or_default()
                .trim_end_matches('/');
            assert!(
                stem.starts_with('/'),
                "section {id} has no route in its heading, found {route:?}"
            );

            let example = section
                .split_once(r#"<pre class="blob">curl "#)
                .map(|(_, rest)| rest.split_once("</pre>").unwrap_or((rest, "")).0)
                .unwrap_or_else(|| panic!("section {id} documents no curl example"));
            assert!(
                example.contains(stem),
                "the example under {id} does not call {stem}: {example}"
            );
            checked += 1;
        }

        assert!(
            checked >= 13,
            "only {checked} endpoint sections were examined, so this test is \
             not reading the page"
        );
    }

    fn ring_at(heights: &[u64]) -> Vec<RingView> {
        heights
            .iter()
            .map(|&h| RingView {
                height: h,
                public_key: "2".repeat(64),
                tx_hash: "3".repeat(64),
            })
            .collect()
    }

    /// The strip runs oldest-left, spend-right.
    #[test]
    fn the_oldest_member_anchors_the_left_edge_and_the_newest_the_spend() {
        let spent_at = 3_000_000;
        let axis = 40 * BLOCKS_PER_DAY;
        let strip = age_strip(&ring_at(&[spent_at, spent_at - axis]), spent_at, axis);

        assert_eq!(
            strip.marks.first().map(|m| m.stem),
            Some(STRIP_WIDTH - STEM_WIDTH),
            "a member as new as the spend sits at the right edge"
        );
        assert_eq!(
            strip.marks.last().map(|m| m.stem),
            Some(0),
            "and the oldest at the left"
        );
    }

    /// Older is always further left. Nothing else on the strip means anything
    /// if this does not hold.
    #[test]
    fn marks_run_in_age_order_along_the_strip() {
        let spent_at = 3_000_000;
        let heights: Vec<u64> = [0, BLOCKS_PER_HOUR, BLOCKS_PER_DAY, 30 * BLOCKS_PER_DAY]
            .iter()
            .map(|age| spent_at - age)
            .collect();
        let strip = age_strip(&ring_at(&heights), spent_at, 30 * BLOCKS_PER_DAY);

        let mut checked = 0;
        for pair in strip.marks.windows(2) {
            if let [newer, older] = pair {
                assert!(
                    older.stem < newer.stem,
                    "an older member drawn at {} is not left of a newer one at {}",
                    older.stem,
                    newer.stem
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 3, "every neighbouring pair was compared");
    }

    /// The point of the log axis: a real ring clusters in the last day while
    /// one member may be a year old. A linear axis would put those two recent
    /// members less than a unit apart.
    #[test]
    fn a_recent_cluster_stays_legible_beside_a_year_old_member() {
        let spent_at = 3_000_000;
        let strip = age_strip(
            &ring_at(&[
                spent_at - BLOCKS_PER_HOUR,
                spent_at - 6 * BLOCKS_PER_HOUR,
                spent_at - 365 * BLOCKS_PER_DAY,
            ]),
            spent_at,
            365 * BLOCKS_PER_DAY,
        );

        let (one_hour, six_hours) = (
            strip.marks.first().map_or(0, |m| m.stem),
            strip.marks.get(1).map_or(0, |m| m.stem),
        );
        assert!(
            one_hour.abs_diff(six_hours) > HALO_WIDTH,
            "one hour and six hours are {} apart, so their halos merge into one",
            one_hour.abs_diff(six_hours)
        );
    }

    /// The halo stands for its member's age, so it has to sit around that age
    /// rather than beside it: offset by half a halo, every cluster on the
    /// strip is drawn newer than the ring it came from.
    #[test]
    fn a_halo_is_centred_on_the_member_it_belongs_to() {
        let spent_at = 3_000_000;
        let strip = age_strip(
            &ring_at(&[spent_at - BLOCKS_PER_DAY, spent_at - 365 * BLOCKS_PER_DAY]),
            spent_at,
            365 * BLOCKS_PER_DAY,
        );

        let mark = strip.marks.first().expect("the day-old member");
        assert_eq!(mark.halo + HALO_WIDTH / 2, mark.stem + STEM_WIDTH / 2);
    }

    /// A strip two days wide must not carry a "1y" label with nothing under it.
    #[test]
    fn the_axis_is_labelled_only_across_the_range_it_covers() {
        let spent_at = 3_000_000;
        let strip = age_strip(
            &ring_at(&[spent_at, spent_at - 2 * BLOCKS_PER_DAY]),
            spent_at,
            2 * BLOCKS_PER_DAY,
        );

        let labels: Vec<&str> = strip.ticks.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["1h", "6h", "1d"]);
        assert!(
            strip.ticks.iter().all(|t| t.x <= STRIP_WIDTH),
            "a tick fell outside the strip it labels"
        );
    }

    #[test]
    fn the_axis_reaches_the_oldest_member_of_any_input() {
        let spent_at = 3_000_000;
        assert_eq!(
            axis_span(
                [spent_at - 10, spent_at - 4000, spent_at - 700].into_iter(),
                spent_at
            ),
            4000
        );
        assert_eq!(
            axis_span([spent_at + 50].into_iter(), spent_at),
            0,
            "a member newer than the spend does not stretch the axis backwards"
        );
        assert_eq!(axis_span([].into_iter(), spent_at), 0);
    }

    /// Every input of a transaction is drawn against the same axis, so the
    /// same cluster sits in the same place on each. An input whose ring is
    /// younger than the widest one therefore stops short of the left edge.
    #[test]
    fn an_input_is_drawn_against_the_transactions_axis_not_its_own() {
        let spent_at = 3_000_000;
        let ring = ring_at(&[spent_at, spent_at - BLOCKS_PER_DAY]);

        let alone = age_strip(&ring, spent_at, BLOCKS_PER_DAY);
        let beside_an_older_input = age_strip(&ring, spent_at, 365 * BLOCKS_PER_DAY);

        assert_eq!(alone.marks.last().map(|m| m.stem), Some(0));
        assert!(
            beside_an_older_input.marks.last().map_or(0, |m| m.stem) > HALO_WIDTH,
            "a day-old member is drawn as though it were the oldest on the page"
        );
    }

    #[test]
    fn an_age_is_labelled_in_the_unit_a_reader_would_use() {
        assert_eq!(age_label(0), "0 min");
        assert_eq!(age_label(5), "10 min");
        assert_eq!(age_label(BLOCKS_PER_HOUR), "1 h");
        assert_eq!(age_label(BLOCKS_PER_DAY), "1 d");
        assert_eq!(age_label(29 * BLOCKS_PER_DAY), "29 d");
        assert_eq!(age_label(45 * BLOCKS_PER_DAY), "1 mo");
    }

    /// A ring member mined after the spending block would underflow an
    /// unchecked subtraction. The daemon should never return one; the page
    /// must not render a wrong chart if it does.
    #[test]
    fn a_ring_member_newer_than_the_spend_does_not_wrap() {
        let strip = age_strip(&ring_at(&[3_000_100]), 3_000_000, 0);

        assert_eq!(
            strip.marks.first().map(|m| m.stem),
            Some(STRIP_WIDTH - STEM_WIDTH),
            "it reads as brand new rather than as ancient"
        );
        assert_eq!(strip.marks.len(), 1);
        assert!(strip.ticks.is_empty(), "there is no age range to label");
    }

    /// The info icon in the caption is an `<svg>` inside `.ring-ages` as well,
    /// so a rule meant for the strip has to name the strip. Written as
    /// `.ring-ages svg`, it stretched a 13px icon to the width of the page.
    #[test]
    fn the_strip_is_styled_by_its_own_class_rather_than_by_being_an_svg() {
        let html = tx_page().render().expect("renders");
        assert!(
            html.contains(r#"<svg class="strip""#),
            "the strip does not carry the class its rules are written for"
        );
        assert!(
            !STYLESHEET.contains(".ring-ages svg"),
            "a rule for every svg under .ring-ages also sizes the caption icon"
        );
    }

    /// The tag states what was inferred, not the output count.
    ///
    /// It first read "51 recipients", which the Outputs column beside it
    /// already said -- a tag that repeats an adjacent cell costs a reader
    /// attention and tells them nothing.
    #[test]
    fn the_pool_payout_tag_says_more_than_the_output_count_does() {
        let html = block_page().render().expect("renders");
        assert!(
            html.contains(r#"<span class="tag coinbase">pool payout</span>"#),
            "the inference is not stated:\n{html}"
        );

        let outputs = block_tx(true).outputs;
        assert!(
            !html.contains(&format!(r#"<span class="tag coinbase">{outputs}"#)),
            "the tag opens by restating the output count"
        );

        let mut tx = tx_page();
        tx.coinbase = true;
        tx.pool_payout = true;
        let tx_html = tx.render().expect("renders");
        assert!(tx_html.contains(r#"<span class="tag coinbase">pool payout</span>"#));
        assert!(
            !tx_html.contains("recipients</span>"),
            "the transaction heading still counts recipients in its tag"
        );
    }

    /// A coinbase paying many recipients is the shape a decentralised pool
    /// leaves. Reported as a shape, not as an attribution to any software.
    #[test]
    fn only_a_multi_output_coinbase_reads_as_a_pool_payout() {
        assert!(is_pool_payout(true, 51), "p2pool-shaped coinbase");
        assert!(!is_pool_payout(true, 1), "solo or custodial pool coinbase");
        assert!(
            !is_pool_payout(false, 51),
            "an ordinary transaction with many outputs is not a payout"
        );
        assert!(!is_pool_payout(false, 2));
    }

    /// The hints are plain markup: no script, no external reference, and they
    /// work with JavaScript off, which is the only way they can work here.
    #[test]
    fn the_transaction_page_hints_need_no_script() {
        let mut page = tx_page();
        page.payment_id8 = "1234567890abcdef".to_owned();
        let html = page.render().expect("renders");

        assert!(
            html.matches("<details class=\"hint\">").count() >= 6,
            "the explanatory hints are missing:\n{html}"
        );
        // An icon, not a bare "?" character: the glyph relied on the reader
        // guessing, and rendered at the mercy of whatever font was in use.
        assert!(
            !html.contains("<summary>?</summary>"),
            "the hint affordance is a bare question mark again"
        );
        assert_eq!(
            html.matches(r#"<svg class="icon""#).count(),
            html.matches("<details class=\"hint\">").count(),
            "every hint should carry the icon"
        );
        assert!(!html.to_lowercase().contains("<script"));
        assert!(!html.contains("onclick"));
        // The key image was an unlabelled hash next to the ring member count,
        // which is what made it read as an output key.
        assert!(html.contains("Key image"), "the key image is unlabelled");
        assert!(
            html.contains("tx_extra"),
            "the Extra heading does not say what it is"
        );
    }

    /// No page may carry a `style=` attribute.
    ///
    /// The policy is `style-src 'self'` with no `'unsafe-inline'`, so a
    /// browser drops inline styles silently -- the markup looks right, the
    /// rule never applies, and nothing reports it. Three had accumulated this
    /// way before this test existed.
    #[test]
    fn no_page_styles_itself_inline() {
        let mut tx = tx_page();
        tx.pool_payout = true;
        tx.pruned = true;
        tx.extra_fields = vec![ExtraField {
            name: "Transaction public key".to_owned(),
            value: "a".repeat(64),
        }];
        tx.inputs.iter_mut().for_each(|i| i.unavailable = true);

        for (name, html) in [
            ("index", index_page().render().expect("renders")),
            ("block", block_page().render().expect("renders")),
            ("tx", tx.render().expect("renders")),
            ("api", api_page().render().expect("renders")),
            (
                "mempool",
                mempool_page(Some((SortKey::Size, SortDir::Desc)))
                    .render()
                    .expect("renders"),
            ),
        ] {
            assert!(
                !html.contains("style=\""),
                "{name} carries an inline style, which the policy discards:\n{html}"
            );
        }
    }

    /// The stylesheet link carries a key derived from the stylesheet itself.
    ///
    /// Without it, the day-long `max-age` means a returning browser renders
    /// new markup against an old stylesheet. That is not hypothetical: it is
    /// how the `<details>` hints first appeared, as a bare disclosure triangle
    /// beside a solid black circle, because the cached CSS predated the rules
    /// that style them.
    #[test]
    fn the_stylesheet_url_changes_when_the_stylesheet_does() {
        // The published FNV-1a 64-bit vectors. Pinned because the doc comment
        // claims this *is* FNV-1a, and the first version of it was not: the
        // multiplier was written 0x1000_0000_01b3, one digit longer than the
        // real prime, which still hashed but was not the named algorithm.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);

        // The key tracks content: two different stylesheets cannot share one.
        assert_ne!(fnv1a(b"a { color: red }"), fnv1a(b"a { color: blue }"));
        assert_ne!(fnv1a(b""), fnv1a(b" "));
        assert_eq!(fnv1a(b"same"), fnv1a(b"same"));
        assert_ne!(STYLESHEET_VERSION, 0);

        let expected = format!("/static/style.css?v={STYLESHEET_VERSION}");
        for (name, html) in [
            ("index", index_page().render().expect("renders")),
            ("block", block_page().render().expect("renders")),
            ("tx", tx_page().render().expect("renders")),
            ("api", api_page().render().expect("renders")),
        ] {
            assert!(
                html.contains(&expected),
                "{name} links the stylesheet without a cache key"
            );
        }
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

    // -----------------------------------------------------------------------
    // The three pages a reader actually spends time on.
    //
    // Every compatibility test in this repository compares `/api/*` output, so
    // nothing above this line ever rendered an index, block or transaction
    // page. Three wrong figures shipped behind that gap: a column headed
    // "Fees" that held the block reward, a block page that counted its own
    // coinbase and so read one higher than the index row linking to it, and a
    // `0.0 XMR` label on every RingCT input. All three are template-and-
    // mapping bugs, invisible to a JSON differential by construction.
    // -----------------------------------------------------------------------

    fn index_page() -> IndexPage {
        IndexPage {
            version: VERSION,
            query: None,
            chain: status(),
            blocks: vec![BlockRow {
                height: 3_185_430,
                age: "00:01:12".to_owned(),
                size: 40_490,
                tx_count: 16,
                reward: "0.60160672".to_owned(),
                hash: "a".repeat(64),
            }],
            page: 0,
        }
    }

    fn block_tx(coinbase: bool) -> BlockTxRow {
        BlockTxRow {
            hash: if coinbase { "c" } else { "d" }.repeat(64),
            coinbase,
            pool_payout: is_pool_payout(coinbase, 2),
            outputs: 2,
            fee: if coinbase { "0.0" } else { "0.00071136" }.to_owned(),
            ring: if coinbase { 0 } else { 16 },
            size: 2_223,
        }
    }

    fn block_page() -> BlockPage {
        let txs = vec![block_tx(true), block_tx(false), block_tx(false)];
        BlockPage {
            version: VERSION,
            query: None,
            chain: status(),
            height: 3_185_430,
            depth: 1,
            hash: "a".repeat(64),
            prev_hash: "b".repeat(64),
            timestamp: 1_790_038_920,
            timestamp_utc: "2026-09-22 01:02:00".to_owned(),
            age: "00:01:12".to_owned(),
            size: 40_490,
            weight: 40_490,
            tx_count: table_tx_count(&txs),
            reward: "0.60160672".to_owned(),
            difficulty: "691253322598".to_owned(),
            nonce: 7,
            major_version: 16,
            minor_version: 16,
            txs,
        }
    }

    fn pool_row(waiting_secs: u64, fee_atomic: u64, size: u64) -> PoolRow {
        PoolRow {
            hash: "e".repeat(64),
            age: "00:00:00".to_owned(),
            waiting_secs,
            fee: "0.0".to_owned(),
            fee_atomic,
            ring: 16,
            size,
        }
    }

    fn mempool_page(active: Option<(SortKey, SortDir)>) -> MempoolPage {
        let txs = vec![
            pool_row(10, 300, 2_000),
            pool_row(30, 100, 1_000),
            pool_row(20, 200, 3_000),
        ];
        MempoolPage {
            version: VERSION,
            query: None,
            chain: status(),
            waiting_sort: column_sort(SortKey::Waiting, active),
            fee_sort: column_sort(SortKey::Fee, active),
            size_sort: column_sort(SortKey::Size, active),
            txs,
        }
    }

    #[test]
    fn waiting_time_is_the_gap_since_the_transaction_was_received() {
        assert_eq!(waiting_secs(1_000, 400), 600);
        assert_eq!(
            waiting_secs(400, 1_000),
            600,
            "the same gap either way round"
        );
        assert_eq!(waiting_secs(500, 500), 0);
    }

    #[test]
    fn the_query_string_is_parsed_into_a_sort_that_defaults_to_descending() {
        assert_eq!(
            active_sort(Some("fee"), Some("asc")),
            Some((SortKey::Fee, SortDir::Asc))
        );
        assert_eq!(
            active_sort(Some("fee"), None),
            Some((SortKey::Fee, SortDir::Desc)),
            "no dir defaults to descending"
        );
        assert_eq!(
            active_sort(Some("fee"), Some("sideways")),
            Some((SortKey::Fee, SortDir::Desc)),
            "a bad dir falls back to descending rather than failing the page"
        );
        assert_eq!(
            active_sort(Some("hash"), Some("asc")),
            None,
            "hash has no useful order to sort by"
        );
        assert_eq!(
            active_sort(None, Some("asc")),
            None,
            "a dir with no sort names nothing to reverse"
        );
    }

    #[test]
    fn sort_key_and_dir_round_trip_through_their_query_strings() {
        for k in [SortKey::Waiting, SortKey::Fee, SortKey::Size] {
            assert_eq!(SortKey::parse(k.as_str()), Some(k));
        }
        assert_eq!(SortKey::parse("hash"), None, "hash has no useful order");
        assert_eq!(
            SortKey::parse("ring"),
            None,
            "sorting by ring was not asked for"
        );

        for d in [SortDir::Asc, SortDir::Desc] {
            assert_eq!(SortDir::parse(d.as_str()), Some(d));
        }
        assert_eq!(SortDir::parse("sideways"), None);
    }

    /// An unsorted column links to itself descending -- largest, or
    /// longest-waiting, first -- and carries no arrow, because nothing is
    /// active yet to point in a direction.
    #[test]
    fn an_unsorted_column_links_to_itself_descending_with_no_arrow() {
        let c = column_sort(SortKey::Fee, None);
        assert_eq!(c.href, "/mempool?sort=fee&dir=desc");
        assert_eq!(c.arrow, "");

        let c = column_sort(SortKey::Size, Some((SortKey::Fee, SortDir::Asc)));
        assert_eq!(
            c.href, "/mempool?sort=size&dir=desc",
            "a column sorted by something else is still unsorted itself"
        );
        assert_eq!(c.arrow, "");
    }

    /// The active column links to its own reverse, so a second click flips
    /// it, and its arrow names the direction it is sorted in right now.
    #[test]
    fn the_active_column_links_to_its_reverse_and_names_its_direction() {
        let desc = column_sort(SortKey::Waiting, Some((SortKey::Waiting, SortDir::Desc)));
        assert_eq!(desc.href, "/mempool?sort=waiting&dir=asc");
        assert_eq!(desc.arrow, " \u{25bc}");

        let asc = column_sort(SortKey::Waiting, Some((SortKey::Waiting, SortDir::Asc)));
        assert_eq!(asc.href, "/mempool?sort=waiting&dir=desc");
        assert_eq!(asc.arrow, " \u{25b2}");
    }

    #[test]
    fn rows_sort_by_the_requested_column_in_the_requested_direction() {
        let mut rows = vec![
            pool_row(10, 300, 2_000),
            pool_row(30, 100, 1_000),
            pool_row(20, 200, 3_000),
        ];

        sort_pool_rows(&mut rows, SortKey::Fee, SortDir::Asc);
        assert_eq!(
            rows.iter().map(|r| r.fee_atomic).collect::<Vec<_>>(),
            vec![100, 200, 300]
        );

        sort_pool_rows(&mut rows, SortKey::Waiting, SortDir::Desc);
        assert_eq!(
            rows.iter().map(|r| r.waiting_secs).collect::<Vec<_>>(),
            vec![30, 20, 10]
        );

        sort_pool_rows(&mut rows, SortKey::Size, SortDir::Asc);
        assert_eq!(
            rows.iter().map(|r| r.size).collect::<Vec<_>>(),
            vec![1_000, 2_000, 3_000]
        );
    }

    /// A stable sort: rows tied on the sort key keep the daemon's own order,
    /// whichever direction was asked for, rather than flipping arbitrarily.
    #[test]
    fn rows_tied_on_the_sort_key_keep_their_original_order() {
        let mut rows = vec![
            pool_row(5, 100, 1),
            pool_row(5, 200, 2),
            pool_row(5, 300, 3),
        ];
        sort_pool_rows(&mut rows, SortKey::Waiting, SortDir::Desc);
        assert_eq!(
            rows.iter().map(|r| r.fee_atomic).collect::<Vec<_>>(),
            vec![100, 200, 300]
        );
    }

    #[test]
    fn the_mempool_headers_link_to_the_sort_state_they_were_given() {
        let html = mempool_page(Some((SortKey::Fee, SortDir::Asc)))
            .render()
            .expect("renders");
        assert!(
            html.contains(r#"href="/mempool?sort=fee&#38;dir=desc""#),
            "the active column should link to its own reverse:\n{html}"
        );
        assert!(
            html.contains("Fee \u{25b2}"),
            "the active column should show which way it is sorted:\n{html}"
        );
        assert!(
            html.contains(r#"href="/mempool?sort=waiting&#38;dir=desc""#),
            "an inactive column should default to descending:\n{html}"
        );
        assert!(
            !html.contains("Waiting [h:m:s] \u{25b2}")
                && !html.contains("Waiting [h:m:s] \u{25bc}"),
            "an inactive column must not carry an arrow:\n{html}"
        );
    }

    /// One RingCT input and output, one pre-RingCT input and output. Both
    /// states have to be present or the distinction is unobservable.
    fn tx_page() -> TxPage {
        TxPage {
            version: VERSION,
            query: None,
            chain: status(),
            hash: "e".repeat(64),
            coinbase: false,
            pool_payout: false,
            in_pool: false,
            pruned: false,
            block_height: 3_185_430,
            confirmations: 1,
            timestamp: 1_790_038_920,
            timestamp_utc: "2026-09-22 01:02:00".to_owned(),
            age: "00:01:12".to_owned(),
            fee: "0.00071136".to_owned(),
            size: 2_223,
            version_no: 2,
            rct_type: 6,
            ring_size: 16,
            unlock_time: 0,
            payment_id: String::new(),
            payment_id8: String::new(),
            inputs: vec![
                InputView {
                    key_image: "1".repeat(64),
                    amount: visible_amount(0),
                    unavailable: false,
                    ring: vec![RingView {
                        height: 3_100_000,
                        public_key: "2".repeat(64),
                        tx_hash: "3".repeat(64),
                    }],
                    ages: age_strip(&[], 0, 0),
                },
                InputView {
                    key_image: "4".repeat(64),
                    amount: visible_amount(2_000_000_000_000),
                    unavailable: false,
                    ring: Vec::new(),
                    ages: age_strip(&[], 0, 0),
                },
            ],
            outputs: vec![
                OutputView {
                    public_key: "5".repeat(64),
                    amount: visible_amount(0),
                    view_tag: "94".to_owned(),
                },
                OutputView {
                    public_key: "6".repeat(64),
                    amount: visible_amount(3_000_000_000_000),
                    view_tag: "d6".to_owned(),
                },
            ],
            has_view_tags: true,
            extra: "01aa".to_owned(),
            extra_fields: Vec::new(),
            extra_undecoded: false,
        }
    }

    /// A RingCT amount is hidden, not zero, and the two must not render alike.
    ///
    /// The whole rule lives here because it used to live in two places written
    /// two different ways: the output path compared against a `"0"` sentinel it
    /// built itself, the input path compared against `"0"` but formatted
    /// through `xmr`, which never produces `"0"` -- it produces `"0.0"`.
    #[test]
    fn a_hidden_amount_has_no_string_form() {
        assert_eq!(visible_amount(0), None);
        assert_eq!(
            visible_amount(2_000_000_000_000),
            Some("2.0".to_owned()),
            "a pre-RingCT amount is public and must still be shown"
        );
        assert_eq!(visible_amount(1), Some("0.000000000001".to_owned()));
        // The trap: the formatter's rendering of zero is not the digit zero.
        assert_eq!(xmr(0), "0.0");
    }

    /// A fee column mixes tiny fees and large ones; trimmed, `xmr` gives them
    /// different numbers of decimal places and a right-aligned column stops
    /// lining up on the decimal point. `xmr_aligned` always shows all twelve.
    #[test]
    fn a_column_amount_keeps_every_decimal_place_so_the_column_aligns() {
        assert_eq!(xmr_aligned(0), "0.000000000000");
        assert_eq!(xmr_aligned(600_000_000_000), "0.600000000000");
        assert_eq!(
            xmr_aligned(30_600),
            "0.000000030600",
            "trimmed, this would be shorter than the row above and misalign"
        );
    }

    /// Bulletproofs hide the amount; the page must not print a figure for it.
    #[test]
    fn a_ringct_input_or_output_shows_no_number() {
        let html = tx_page().render().expect("renders");

        assert!(
            !html.contains("0.0 XMR"),
            "a RingCT input was labelled with an amount:\n{html}"
        );
        assert!(
            html.contains("2.0 XMR"),
            "the pre-RingCT input's visible amount was dropped"
        );
        // The output column says so in words rather than printing a zero.
        assert_eq!(
            html.matches(r#"<span class="tag">hidden</span>"#).count(),
            1,
            "exactly one of the two outputs is a hidden RingCT amount"
        );
        assert!(
            html.contains("3.0"),
            "the pre-RingCT output amount is shown"
        );
    }

    /// `num_txes` counts non-coinbase transactions only, but the block page
    /// counts every rendered row. A block reading 16 on the index used to
    /// read 17 once opened, because the two pages counted differently and
    /// nothing tied them together. They now agree because both mean "every
    /// transaction, coinbase included" -- the index computes that total from
    /// `num_txes` since it never fetches the block body, and the block page
    /// simply counts what it renders. The coinbase is not called out a
    /// second time in the count: the table directly beneath it already
    /// tags which row is the coinbase.
    #[test]
    fn the_index_and_the_block_page_count_transactions_the_same_way() {
        assert_eq!(total_tx_count(29), 30, "num_txes plus the one coinbase");
        assert_eq!(total_tx_count(0), 1, "a coinbase-only block is still one");
        assert_eq!(
            total_tx_count(u64::MAX),
            u64::MAX,
            "saturates rather than wrapping past the header's own type"
        );

        // 29 non-coinbase transactions plus the coinbase itself, counted
        // through the block handler's own `table_tx_count` rather than
        // recomputed here -- recomputing `txs.len()` a second time would
        // pass even if the handler's copy silently excluded the coinbase
        // again, since both copies would agree with each other and with
        // nothing else.
        let mut txs = vec![block_tx(true)];
        txs.extend((0..29).map(|_| block_tx(false)));
        assert_eq!(txs.len(), 30, "fixture is 1 coinbase + 29 others");

        let mut page = block_page();
        page.tx_count = table_tx_count(&txs);
        page.txs = txs;

        assert_eq!(
            u64::try_from(page.tx_count).expect("small count"),
            total_tx_count(29),
            "the block page's own total disagrees with the index's"
        );

        let html = page.render().expect("renders");
        assert!(
            html.contains("<dt>Transactions</dt><dd>30</dd>"),
            "the page does not show the plain total:\n{html}"
        );
        assert!(
            !html.contains("coinbase</dd>"),
            "the count restates the coinbase, which the table below it \
             already tags:\n{html}"
        );
    }

    /// No header field carries the base emission, so the fee total of a block
    /// cannot be computed from one. A column headed "Fees" on this page is
    /// therefore always either the reward under a wrong name -- which is what
    /// it was -- or a number that was invented.
    #[test]
    fn the_front_page_does_not_claim_to_show_fees() {
        let html = index_page().render().expect("renders");
        assert!(html.contains(r#"<th class="num">Reward</th>"#));
        assert!(
            !html.to_lowercase().contains("fee"),
            "the front page names a fee it cannot compute:\n{html}"
        );
        assert!(html.contains("0.60160672"), "the reward value is shown");
    }

    /// Escaping and the content policy, checked on the pages that carry chain
    /// data rather than only on the error page.
    #[test]
    fn the_data_pages_escape_their_input_and_reference_nothing_external() {
        let hostile = r#"<script>alert(1)</script>"#.to_owned();

        let mut index = index_page();
        index.query = Some(hostile.clone());
        let mut block = block_page();
        block.query = Some(hostile.clone());
        let mut tx = tx_page();
        tx.query = Some(hostile.clone());
        tx.payment_id = hostile.clone();

        for (name, html) in [
            ("index", index.render().expect("renders")),
            ("block", block.render().expect("renders")),
            ("tx", tx.render().expect("renders")),
        ] {
            let lower = html.to_lowercase();
            assert!(!lower.contains("<script"), "{name} emitted a script tag");
            assert!(!lower.contains("javascript:"), "{name} emitted a js url");
            assert!(!lower.contains("<img"), "{name} emitted an image");
            assert!(!lower.contains("http://"), "{name} left the origin");
            assert!(!lower.contains("https://"), "{name} left the origin");
            assert_eq!(
                lower.matches("/static/style.css").count(),
                1,
                "{name} does not load exactly one stylesheet"
            );
            assert!(
                html.contains("&#60;script&#62;"),
                "{name} did not render the escaped form, so this test is not \
                 seeing the hostile value at all"
            );
        }
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
