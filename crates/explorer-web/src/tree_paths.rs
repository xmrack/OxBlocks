//! A transaction's outputs' paths through the curve tree, fetched, placed and
//! checked: what the paths page and `/api/transaction/<hash>/paths` show.

use std::ops::Range;

use explorer_core::ChainError;
use explorer_core::curve_tree::{PathCheck, PlacedPath, place_all};
use monerod_rpc::types::{PathQuery, TxEntry, TxJson, last_locked_block};

use crate::api::handlers::AppState;

/// The most outputs shown at once: the most one daemon call answers for.
pub const MAX_OUTPUTS: usize = PathQuery::MAX_IDS;

/// Paths as of one block, for some of one transaction's outputs.
pub struct TxPaths {
    pub as_of_block: u64,
    /// The chain's tip when asked, the newest block a path can be taken as of.
    pub tip: u64,
    pub n_leaf_tuples: u64,
    /// The block carrying the root of the tree as of `as_of_block`, eight
    /// blocks below it, with that root, where the block carries one.
    pub root_block: Option<(u64, String)>,
    pub outputs: Vec<OutputPath>,
}

pub struct OutputPath {
    /// Counted from 0, as the transaction lists its outputs.
    pub index: usize,
    pub unified_id: u64,
    /// The last block before the output joins the tree: `None` from `placed`
    /// as of any block below this one.
    pub last_locked_block: u64,
    /// `None` when the output is not in the tree as of the block asked about.
    pub placed: Option<PlacedPath>,
}

/// What every path found says about the root, against the block's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootCheck {
    /// Every path's hashes hold and end at the root the block records.
    Matches,
    /// Some path's hashes do not hold, or end at another root.
    Fails,
    /// No block carries the root to compare with, or no output is in the
    /// tree yet.
    Unchecked,
}

impl TxPaths {
    #[must_use]
    pub fn root_check(&self) -> RootCheck {
        let placed: Vec<&PlacedPath> = self
            .outputs
            .iter()
            .filter_map(|o| o.placed.as_ref())
            .collect();
        if placed.iter().any(|p| p.check != PathCheck::Holds) {
            return RootCheck::Fails;
        }
        let Some((_, root)) = &self.root_block else {
            return RootCheck::Unchecked;
        };
        if placed.is_empty() {
            return RootCheck::Unchecked;
        }
        if placed.iter().all(|p| {
            p.root()
                .is_some_and(|r| explorer_core::hex::encode(r).eq_ignore_ascii_case(root))
        }) {
            RootCheck::Matches
        } else {
            RootCheck::Fails
        }
    }
}

/// Why there are no paths to show.
pub enum PathsError {
    /// The transaction is in the pool: its outputs have no place in the
    /// chain's order of outputs yet, so none in the tree.
    InPool,
    /// The daemon gives the transaction's outputs no unified ids: it is from
    /// before FCMP++.
    NoIds,
    /// The block asked about is past the tip.
    Ahead {
        asked: u64,
        tip: u64,
    },
    Chain(ChainError),
}

/// The paths of `tx`'s outputs in `which`, as of `as_of` or the tip.
///
/// `which` is clamped to the outputs the transaction has and to
/// [`MAX_OUTPUTS`] of them.
pub async fn gather(
    state: &AppState,
    entry: &TxEntry,
    tx: &TxJson,
    as_of: Option<u64>,
    which: Range<usize>,
) -> Result<TxPaths, PathsError> {
    if entry.in_pool {
        return Err(PathsError::InPool);
    }
    let ids = entry
        .unified_ids_per_output(tx.vout.len())
        .ok_or(PathsError::NoIds)?;
    let info = state.chain.info().await.map_err(PathsError::Chain)?;
    let tip = info.height.saturating_sub(1);
    let as_of_block = as_of.unwrap_or(tip);
    if as_of_block > tip {
        return Err(PathsError::Ahead {
            asked: as_of_block,
            tip,
        });
    }
    let end = which
        .end
        .min(ids.len())
        .min(which.start.saturating_add(MAX_OUTPUTS));
    let start = which.start.min(end);
    let wanted = ids.get(start..end).unwrap_or_default().to_vec();
    if wanted.is_empty() {
        return Ok(TxPaths {
            as_of_block,
            tip,
            n_leaf_tuples: 0,
            root_block: None,
            outputs: Vec::new(),
        });
    }

    let (answer, root_block) = tokio::join!(
        state.chain.tree_paths(as_of_block, &wanted),
        state.chain.proof_root(as_of_block),
    );
    let answer = answer.map_err(PathsError::Chain)?;
    let n_leaf_tuples = answer.n_leaf_tuples;

    // Checking a path is CPU work, a few milliseconds a group of leaves, so
    // it is kept off the threads serving other requests.
    let ids_for_check = wanted.clone();
    let placed =
        tokio::task::spawn_blocking(move || place_all(&ids_for_check, answer.paths, n_leaf_tuples))
            .await
            .map_err(|e| {
                PathsError::Chain(ChainError::BadAnswer {
                    what: PathQuery::ENDPOINT,
                    detail: format!("checking the paths stopped: {e}"),
                })
            })?;

    let locked = last_locked_block(tx.unlock_time, entry.block_height);
    let outputs = placed
        .into_iter()
        .zip(wanted)
        .enumerate()
        .map(|(k, (placed, unified_id))| OutputPath {
            index: start + k,
            unified_id,
            last_locked_block: locked,
            placed,
        })
        .collect();
    Ok(TxPaths {
        as_of_block,
        tip,
        n_leaf_tuples,
        root_block,
        outputs,
    })
}
