//! `/tx/<hash>/paths`: where a transaction's outputs sit in the curve tree,
//! one output at a time or all of them together.

use std::collections::BTreeMap;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use explorer_core::curve_tree::{Curve, Group, PathCheck, PlacedPath, group_width};
use monerod_rpc::types::{LeafKind, PathLeaf};

use super::{
    ChainStatus, Page, TreeFunnel, VERSION, chain_error_page, error_page, fetch_tx, grouped,
    mark_paths, render, status_of, tree_picture,
};
use crate::api::handlers::Shared;
use crate::tree_paths::{MAX_OUTPUTS, PathsError, RootCheck, TxPaths, gather};

/// Bytes a wallet keeps per leaf of a path, its key and commitment, and per
/// point above.
const LEAF_BYTES: u64 = 64;
const POINT_BYTES: u64 = 32;

#[derive(serde::Deserialize)]
pub struct PathsParams {
    /// One output, counted from 1 as the transaction page lists them. All of
    /// them when absent.
    output: Option<usize>,
    /// The block to take the tree as of. The tip when absent.
    block: Option<u64>,
    /// The first output of the window shown together, counted from 0, for a
    /// transaction with more outputs than one call answers for.
    from: Option<usize>,
}

#[derive(Template)]
#[template(path = "paths.html")]
struct PathsPage {
    version: &'static str,
    query: Option<String>,
    chain: Option<ChainStatus>,
    hash: String,
    as_of: u64,
    tip: u64,
    /// The block asked about, when it is not the tip, for the form that
    /// changes it.
    block: Option<u64>,
    leaves: String,
    n_layers: usize,
    root_block: Option<(u64, String)>,
    root_check: &'static str,
    outputs_total: usize,
    /// The output shown alone, counted from 1, or `None` for all of them.
    showing: Option<usize>,
    tabs: Vec<Tab>,
    windows: Vec<Tab>,
    statuses: Vec<StatusRow>,
    tree: Option<[TreeFunnel; 2]>,
    /// From the root down, the leaves' parents last.
    layers: Vec<LayerView>,
    leaf_groups: Vec<LeafGroupView>,
    /// What a wallet keeps for the paths shown, and for the same paths kept
    /// apart, when more than one is shown and they share groups.
    stored: Option<String>,
    stored_apart: Option<String>,
}

struct Tab {
    label: String,
    href: String,
    on: bool,
}

struct StatusRow {
    label: String,
    href: String,
    unified_id: u64,
    /// Where the output sits among the leaves, grouped, when it is in the tree.
    leaf: Option<String>,
    /// The first block whose tree holds it, when it is not in this one.
    joins: Option<u64>,
    check: &'static str,
}

struct LayerView {
    name: String,
    curve: &'static str,
    groups: Vec<GroupView>,
}

struct GroupView {
    caption: String,
    members: Vec<Chip>,
}

struct Chip {
    short: String,
    full: String,
    on: bool,
}

struct LeafGroupView {
    caption: String,
    rows: Vec<LeafRow>,
}

struct LeafRow {
    position: String,
    unified_id: u64,
    kind: &'static str,
    key: String,
    commitment: String,
    /// This transaction's output, counted from 1, when the leaf is one.
    output: Option<usize>,
}

pub async fn tree_paths(
    State(state): Shared,
    Path(raw): Path<String>,
    Query(q): Query<PathsParams>,
) -> Page {
    let mut chain = status_of(&state).await;
    let (entry, tx) = match fetch_tx(&state, &mut chain, &raw).await {
        Ok(found) => found,
        Err(page) => return page,
    };
    let hash = entry.tx_hash.to_lowercase();
    let total = tx.vout.len();

    let which = match q.output {
        Some(k) if k == 0 || k > total => {
            return error_page(
                chain,
                StatusCode::NOT_FOUND,
                "No such output",
                &format!("Transaction {hash} has {total} output{}.", plural(total)),
            );
        }
        Some(k) => (k - 1)..k,
        None => {
            let from = q.from.unwrap_or(0) / MAX_OUTPUTS * MAX_OUTPUTS;
            from..from.saturating_add(MAX_OUTPUTS)
        }
    };

    let paths = match gather(&state, &entry, &tx, q.block, which).await {
        Ok(p) => p,
        Err(PathsError::InPool) => {
            return error_page(
                chain,
                StatusCode::NOT_FOUND,
                "Not in the tree yet",
                "This transaction is still in the pool. Its outputs join the curve tree only \
                 once it is mined and they unlock, ten blocks later.",
            );
        }
        Err(PathsError::NoIds) => {
            return error_page(
                chain,
                StatusCode::NOT_FOUND,
                "No curve tree",
                "This explorer's daemon gives no unified ids, which it does from FCMP++ on, \
                 so it has no curve tree to show paths through.",
            );
        }
        Err(PathsError::Ahead { asked, tip }) => {
            return error_page(
                chain,
                StatusCode::NOT_FOUND,
                "No such block",
                &format!(
                    "The chain's tip is block {tip}, so there is no tree as of block {asked}."
                ),
            );
        }
        Err(PathsError::Chain(e)) => {
            return chain_error_page(chain, &e, &format!("No paths for {hash}"));
        }
    };

    render(StatusCode::OK, &page(chain, hash, total, q.output, &paths))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn page(
    chain: Option<ChainStatus>,
    hash: String,
    total: usize,
    showing: Option<usize>,
    paths: &TxPaths,
) -> PathsPage {
    let block = (paths.as_of_block != paths.tip).then_some(paths.as_of_block);
    let base = format!("/tx/{hash}/paths");
    let link = |output: Option<usize>, from: usize| {
        let parts: Vec<String> = [
            output.map(|k| format!("output={k}")),
            (from > 0).then(|| format!("from={from}")),
            block.map(|b| format!("block={b}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            base.clone()
        } else {
            format!("{base}?{}", parts.join("&"))
        }
    };

    // The window of outputs shown together, and the one holding the output
    // shown alone.
    let from = match showing {
        Some(k) => (k - 1) / MAX_OUTPUTS * MAX_OUTPUTS,
        None => paths.outputs.first().map_or(0, |o| o.index),
    };
    let window = from..(from + MAX_OUTPUTS).min(total);
    let mut tabs = vec![Tab {
        label: if total > MAX_OUTPUTS {
            format!("Outputs {}–{}", window.start + 1, window.end)
        } else {
            "All outputs".to_owned()
        },
        href: link(None, from),
        on: showing.is_none(),
    }];
    tabs.extend(window.map(|i| Tab {
        label: format!("Output {}", i + 1),
        href: link(Some(i + 1), 0),
        on: showing == Some(i + 1),
    }));
    let windows = if total > MAX_OUTPUTS {
        (0..total)
            .step_by(MAX_OUTPUTS)
            .map(|start| Tab {
                label: format!("{}–{}", start + 1, (start + MAX_OUTPUTS).min(total)),
                href: link(None, start),
                on: start == from,
            })
            .collect()
    } else {
        Vec::new()
    };

    let statuses = paths
        .outputs
        .iter()
        .map(|o| StatusRow {
            label: format!("Output {}", o.index + 1),
            href: link(Some(o.index + 1), 0),
            unified_id: o.unified_id,
            leaf: o.placed.as_ref().map(|p| grouped(p.path.leaf_idx)),
            joins: o.placed.is_none().then_some(o.last_locked_block),
            check: match o.placed.as_ref().map(|p| p.check) {
                None => "",
                Some(PathCheck::Holds) => "holds",
                Some(PathCheck::Broken { .. }) => "broken",
                Some(PathCheck::Unreadable { .. }) => "unreadable",
                Some(PathCheck::Misshapen) => "misshapen",
            },
        })
        .collect();

    let placed: Vec<&PlacedPath> = paths
        .outputs
        .iter()
        .filter_map(|o| o.placed.as_ref())
        .collect();
    let outputs_by_id: BTreeMap<u64, usize> = paths
        .outputs
        .iter()
        .map(|o| (o.unified_id, o.index + 1))
        .collect();

    let mut tree = (paths.n_leaf_tuples > 0)
        .then(|| {
            tree_picture(
                paths.n_leaf_tuples,
                paths.root_block.as_ref().map(|(_, r)| r.as_str()),
            )
        })
        .flatten();
    if let Some(picture) = tree.as_mut() {
        let groups: Vec<&[Group]> = placed.iter().map(|p| p.groups.as_slice()).collect();
        for funnel in picture.iter_mut() {
            mark_paths(funnel, &groups);
        }
    }

    let (layers, leaf_groups) = union(&placed, &outputs_by_id);
    let (stored, stored_apart) = stored_bytes(&placed);

    PathsPage {
        version: VERSION,
        query: None,
        chain,
        hash,
        as_of: paths.as_of_block,
        tip: paths.tip,
        block,
        leaves: grouped(paths.n_leaf_tuples),
        n_layers: monerod_rpc::types::tree_layers(paths.n_leaf_tuples).len(),
        root_block: paths.root_block.clone(),
        root_check: match paths.root_check() {
            RootCheck::Matches => "matches",
            RootCheck::Fails => "fails",
            RootCheck::Unchecked => "unchecked",
        },
        outputs_total: total,
        showing,
        tabs,
        windows,
        statuses,
        tree,
        layers,
        leaf_groups,
        stored,
        stored_apart,
    }
}

/// A group of the union, its members, and which of them are ancestors.
type Lit<'a> = (&'a Group, &'a [[u8; 32]], Vec<bool>);

/// Every group of every path, each once, the ancestors of the paths lit:
/// the layers above the leaves from the root down, and the groups of leaves.
fn union(
    placed: &[&PlacedPath],
    outputs_by_id: &BTreeMap<u64, usize>,
) -> (Vec<LayerView>, Vec<LeafGroupView>) {
    // (layer, start) -> the group, its members, and which of them are lit.
    let mut groups: BTreeMap<(usize, u64), Lit<'_>> = BTreeMap::new();
    let mut leaves: BTreeMap<u64, (&Group, &[PathLeaf])> = BTreeMap::new();
    for p in placed {
        for g in &p.groups {
            if g.layer == 0 {
                leaves.entry(g.start).or_insert((g, &p.path.leaves));
                continue;
            }
            let Some(members) = p.members(g.layer) else {
                continue;
            };
            let entry = groups
                .entry((g.layer, g.start))
                .or_insert_with(|| (g, members, vec![false; members.len()]));
            if let Some(lit) = usize::try_from(g.offset())
                .ok()
                .and_then(|o| entry.2.get_mut(o))
            {
                *lit = true;
            }
        }
    }

    let depth = placed
        .first()
        .map_or(0, |p| p.groups.len().saturating_sub(1));
    let layers = (1..=depth)
        .rev()
        .map(|layer| LayerView {
            name: if layer == depth {
                "Root".to_owned()
            } else {
                format!("Layer {layer}")
            },
            curve: Curve::of_layer(layer).name(),
            groups: groups
                .range((layer, 0)..=(layer, u64::MAX))
                .map(|(_, (g, members, lit))| GroupView {
                    caption: group_caption(g, "node"),
                    members: members
                        .iter()
                        .zip(lit)
                        .map(|(m, &on)| {
                            let full = explorer_core::hex::encode(m);
                            Chip {
                                short: full.get(..8).unwrap_or_default().to_owned(),
                                full,
                                on,
                            }
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();

    let leaf_groups = leaves
        .values()
        .map(|(g, members)| LeafGroupView {
            caption: group_caption(g, "output"),
            rows: members
                .iter()
                .zip(g.start..)
                .map(|(l, position)| LeafRow {
                    position: grouped(position),
                    unified_id: l.unified_id,
                    kind: match l.kind {
                        LeafKind::Legacy => "legacy",
                        LeafKind::Carrot => "Carrot",
                        LeafKind::Other(_) => "unknown",
                    },
                    key: explorer_core::hex::encode(l.output_key),
                    commitment: explorer_core::hex::encode(l.commitment),
                    output: outputs_by_id.get(&l.unified_id).copied(),
                })
                .collect(),
        })
        .collect();
    (layers, leaf_groups)
}

/// "Group 2 of 2, the last and short: nodes 18–19 of 20", for one group.
/// Groups count from 1 and places in a layer from 0, as unified ids do.
fn group_caption(g: &Group, what: &str) -> String {
    if g.layer_size == 1 {
        return "The root, alone at the top".to_owned();
    }
    let last = g.start + g.len.saturating_sub(1);
    let members = if g.len == 1 {
        format!("{what} {} of {}", grouped(g.start), grouped(g.layer_size))
    } else {
        format!(
            "{what}s {}–{} of {}",
            grouped(g.start),
            grouped(last),
            grouped(g.layer_size)
        )
    };
    let groups = g.groups_in_layer();
    if groups == 1 {
        return format!("One group: {members}");
    }
    let short = if g.len < group_width(g.layer) {
        ", the last and short"
    } else {
        ""
    };
    format!(
        "Group {} of {}{short}: {members}",
        grouped(g.index() + 1),
        grouped(groups)
    )
}

/// What a wallet keeps for these paths together, and, when there is more
/// than one, what it would keep for each on its own.
fn stored_bytes(placed: &[&PlacedPath]) -> (Option<String>, Option<String>) {
    let size = |p: &PlacedPath| {
        LEAF_BYTES * p.path.leaves.len() as u64
            + POINT_BYTES * p.path.layers.iter().map(|l| l.len() as u64).sum::<u64>()
    };
    let apart: u64 = placed.iter().map(|p| size(p)).sum();
    let mut seen = std::collections::BTreeSet::new();
    let together: u64 = placed
        .iter()
        .flat_map(|p| p.groups.iter())
        .filter(|g| seen.insert((g.layer, g.start)))
        .map(|g| if g.layer == 0 { LEAF_BYTES } else { POINT_BYTES } * g.len)
        .sum();
    match placed.len() {
        0 => (None, None),
        1 => (Some(grouped(apart)), None),
        _ => (Some(grouped(together)), Some(grouped(apart))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;
    use crate::tree_paths::OutputPath;
    use monerod_rpc::types::PathQuery;

    const IDS: [u64; 4] = [802, 803, 804, 805];
    const ROOT_806: &str = "e71da88f93a4ded7a2de6217859985fb5349d597e38232572e8d02d8a21e51ce";

    /// The captured transaction's paths as of block `as_of`, as `gather`
    /// would hand them over, with the root block 806 records.
    fn captured(bin: &[u8], as_of: u64, root: &str) -> TxPaths {
        let q = PathQuery::as_of_block(as_of, &IDS).unwrap();
        let answer = q
            .answer(&monerod_rpc::epee::read_root(bin, PathQuery::WANTED).unwrap())
            .unwrap();
        let n = answer.n_leaf_tuples;
        let placed = explorer_core::curve_tree::place_all(&IDS, answer.paths, n);
        TxPaths {
            as_of_block: as_of,
            tip: 814,
            n_leaf_tuples: n,
            root_block: Some((as_of - 8, root.to_owned())),
            outputs: placed
                .into_iter()
                .zip(IDS)
                .enumerate()
                .map(|(index, (placed, unified_id))| OutputPath {
                    index,
                    unified_id,
                    last_locked_block: 810,
                    placed,
                })
                .collect(),
        }
    }

    fn later() -> TxPaths {
        captured(
            include_bytes!("../../../../fixtures/fcmp/paths/get_path_by_unified_id_later.bin"),
            814,
            ROOT_806,
        )
    }

    #[test]
    fn every_path_of_the_captured_transaction_leads_to_its_blocks_root() {
        let paths = later();
        assert_eq!(paths.root_check(), RootCheck::Matches);
        let p = page(None, "ab".to_owned(), 4, None, &paths);
        assert_eq!(p.root_check, "matches");
        assert_eq!(p.n_layers, 3);
        assert_eq!(p.statuses.len(), 4);
        assert!(
            p.statuses
                .iter()
                .all(|s| s.check == "holds" && s.leaf.is_some())
        );

        // The four sit side by side in one group of leaves, so the union has
        // one group per layer, each with one member lit except the leaves.
        assert_eq!(p.leaf_groups.len(), 1);
        let lit: Vec<usize> = p.leaf_groups[0]
            .rows
            .iter()
            .filter_map(|r| r.output)
            .collect();
        assert_eq!(lit, [1, 2, 3, 4]);
        let names: Vec<&str> = p.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Root", "Layer 2", "Layer 1"]);
        for l in &p.layers {
            assert_eq!(l.groups.len(), 1);
            assert_eq!(l.groups[0].members.iter().filter(|c| c.on).count(), 1);
        }
        // Kept apart the four repeat every group; together each is kept once.
        let one = 38 * 64 + (2 + 2 + 1) * 32;
        assert_eq!(p.stored_apart, Some(grouped(4 * one)));
        assert_eq!(p.stored, Some(grouped(one)));

        let html = p.render().unwrap();
        assert!(html.contains("every path leads here"));
        assert!(html.contains("What a wallet keeps"));
        assert_eq!(
            html.matches("class=\"path\"").count(),
            2 * 4,
            "four paths, wide and narrow"
        );
    }

    #[test]
    fn a_path_is_drawn_through_its_ancestor_on_every_bar() {
        // Shown alone, an output is the only one fetched.
        let mut paths = later();
        paths.outputs.truncate(1);
        let p = page(None, "ab".to_owned(), 4, Some(1), &paths);
        let [wide, _] = p.tree.as_ref().unwrap();
        let [mark] = &wide.marks[..] else {
            panic!("one path")
        };
        // Root first in the funnel, leaves last: one dot per bar.
        assert_eq!(mark.dots.len(), 4);
        let root = &wide.rows[0];
        assert_eq!(mark.dots[3].0, root.x + root.width / 2, "the root's middle");
        // The leaf is near the right end of the outputs' bar.
        let leaves = &wide.rows[3];
        assert!(mark.dots[0].0 > leaves.x + leaves.width * 9 / 10);
    }

    #[test]
    fn outputs_not_yet_in_the_tree_say_when_they_join() {
        let paths = captured(
            include_bytes!("../../../../fixtures/fcmp/paths/get_path_by_unified_id_locked.bin"),
            801,
            ROOT_806,
        );
        assert_eq!(paths.root_check(), RootCheck::Unchecked);
        let p = page(None, "ab".to_owned(), 4, None, &paths);
        assert!(
            p.statuses
                .iter()
                .all(|s| s.joins == Some(810) && s.leaf.is_none())
        );
        assert!(p.layers.is_empty() && p.leaf_groups.is_empty());
        let html = p.render().unwrap();
        assert!(html.contains("it joins the tree as of block 810"));
    }

    #[test]
    fn a_root_the_paths_do_not_reach_is_reported() {
        let paths = captured(
            include_bytes!("../../../../fixtures/fcmp/paths/get_path_by_unified_id_later.bin"),
            814,
            &"00".repeat(32),
        );
        assert_eq!(paths.root_check(), RootCheck::Fails);
        let html = page(None, "ab".to_owned(), 4, None, &paths)
            .render()
            .unwrap();
        assert!(html.contains("a path does not lead here"));
    }

    #[test]
    fn links_keep_the_block_and_the_output() {
        let mut paths = later();
        paths.tip = 900;
        let p = page(None, "ab".to_owned(), 4, Some(2), &paths);
        let hrefs: Vec<&str> = p.tabs.iter().map(|t| t.href.as_str()).collect();
        assert_eq!(
            hrefs,
            [
                "/tx/ab/paths?block=814",
                "/tx/ab/paths?output=1&block=814",
                "/tx/ab/paths?output=2&block=814",
                "/tx/ab/paths?output=3&block=814",
                "/tx/ab/paths?output=4&block=814",
            ]
        );
        assert!(p.tabs[2].on);
        assert!(p.windows.is_empty());
    }

    #[test]
    fn captions_count_groups_from_one_and_places_from_zero() {
        let g = Group {
            layer: 1,
            layer_size: 20,
            start: 18,
            len: 2,
            member: 19,
        };
        assert_eq!(
            group_caption(&g, "node"),
            "Group 2 of 2, the last and short: nodes 18–19 of 20"
        );
        let root = Group {
            layer: 3,
            layer_size: 1,
            start: 0,
            len: 1,
            member: 0,
        };
        assert_eq!(group_caption(&root, "node"), "The root, alone at the top");
    }
}
