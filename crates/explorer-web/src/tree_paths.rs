//! A transaction's outputs' paths through the curve tree, fetched, placed and
//! checked: what the paths page and `/api/transaction/<hash>/paths` show.

use std::ops::Range;
use std::sync::LazyLock;

use explorer_core::ChainError;
use explorer_core::curve_tree::{PathCheck, PlacedPath, place_all};
use explorer_core::fmt::decimal;
use monerod_rpc::types::{PathQuery, TxEntry, TxJson, last_locked_block};
use tokio::sync::Semaphore;

use crate::api::handlers::{AppState, echo};

/// The most outputs shown at once: the most one daemon call answers for.
pub const MAX_OUTPUTS: usize = PathQuery::MAX_IDS;

/// How many answers are checked at once, across every request.
///
/// Checking is CPU work on the blocking pool, and it runs to its end even
/// when the request that asked for it has timed out, so it gets its own
/// bound rather than only the daemon calls' one.
const CHECKS_AT_ONCE: usize = 4;

static CHECKS: LazyLock<std::sync::Arc<Semaphore>> =
    LazyLock::new(|| std::sync::Arc::new(Semaphore::new(CHECKS_AT_ONCE)));

/// The query string of the paths page and its API twin, as given.
#[derive(serde::Deserialize, Default)]
pub struct PathsParams {
    output: Option<String>,
    block: Option<String>,
    from: Option<String>,
}

/// [`PathsParams`], read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted {
    /// One output, counted from 1 as the transaction page lists them.
    pub output: Option<usize>,
    /// The block to take the tree as of; the tip when absent.
    pub block: Option<u64>,
    /// The first output of a window, counted from 0.
    pub from: usize,
}

impl PathsParams {
    /// Each parameter is a plain number or absent. An empty one, which the
    /// page's form sends when its box is left blank, is absent.
    pub fn read(&self) -> Result<Wanted, String> {
        fn number(name: &str, given: Option<&str>) -> Result<Option<u64>, String> {
            match given {
                None | Some("") => Ok(None),
                Some(text) => decimal(text)
                    .map(Some)
                    .ok_or_else(|| format!("{name} is not a number: {}", echo(text))),
            }
        }
        let index = |name: &str, given: Option<&str>| -> Result<Option<usize>, String> {
            number(name, given)?
                .map(|n| usize::try_from(n).map_err(|_| format!("{name} is too large: {n}")))
                .transpose()
        };
        Ok(Wanted {
            output: index("output", self.output.as_deref())?,
            block: number("block", self.block.as_deref())?,
            from: index("from", self.from.as_deref())?.unwrap_or(0),
        })
    }
}

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
    /// The block the output joins the tree at: the tree as of this block and
    /// every later one holds it, and `placed` is `None` as of any before it.
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
    /// The window asked for starts past the transaction's last output.
    NoSuchOutputs {
        from: usize,
        total: usize,
    },
    Chain(ChainError),
}

/// The paths of `tx`'s outputs in `which`, as of `as_of` or the tip.
///
/// `which` is clamped to the outputs the transaction has and to
/// [`MAX_OUTPUTS`] of them, and must start at one of them.
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
    let start = which.start;
    let end = which
        .end
        .min(ids.len())
        .min(start.saturating_add(MAX_OUTPUTS));
    let wanted = match ids.get(start..end) {
        Some(w) if !w.is_empty() => w.to_vec(),
        _ => {
            return Err(PathsError::NoSuchOutputs {
                from: start,
                total: ids.len(),
            });
        }
    };

    let (answer, root_block) = tokio::join!(
        state.chain.tree_paths(as_of_block, &wanted),
        state.chain.proof_root(as_of_block),
    );
    let answer = answer.map_err(PathsError::Chain)?;
    let n_leaf_tuples = answer.n_leaf_tuples;

    // Checking a path is CPU work, a few milliseconds a group of leaves, so
    // it is kept off the threads serving other requests. The permit travels
    // with the work and is given back when the work ends, not when a
    // timed-out request stops waiting for it.
    let stopped = |detail: String| {
        PathsError::Chain(ChainError::BadAnswer {
            what: PathQuery::ENDPOINT,
            detail,
        })
    };
    let permit = std::sync::Arc::clone(&CHECKS)
        .acquire_owned()
        .await
        .map_err(|e| stopped(format!("no checking slot: {e}")))?;
    let ids_for_check = wanted.clone();
    let placed = tokio::task::spawn_blocking(move || {
        let placed = place_all(&ids_for_check, answer.paths, n_leaf_tuples);
        drop(permit);
        placed
    })
    .await
    .map_err(|e| stopped(format!("checking the paths stopped: {e}")))?;

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn params(output: Option<&str>, block: Option<&str>, from: Option<&str>) -> PathsParams {
        PathsParams {
            output: output.map(str::to_owned),
            block: block.map(str::to_owned),
            from: from.map(str::to_owned),
        }
    }

    #[test]
    fn an_empty_parameter_is_absent_and_a_bad_one_is_refused() {
        assert_eq!(
            params(Some("2"), Some(""), None).read(),
            Ok(Wanted {
                output: Some(2),
                block: None,
                from: 0
            })
        );
        assert_eq!(
            params(None, Some("814"), Some("50")).read().unwrap().block,
            Some(814)
        );
        assert!(params(None, Some("+8"), None).read().is_err());
        assert!(params(None, None, Some("x")).read().is_err());
        assert!(
            params(Some("99999999999999999999"), None, None)
                .read()
                .is_err()
        );
    }
}
