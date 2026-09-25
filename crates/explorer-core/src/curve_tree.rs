//! Outputs' paths through the FCMP++ curve tree: where each part of a path
//! sits in the tree, and whether its hashes hold up to the root.
//!
//! The daemon hands out a path as bare groups of points (see
//! [`monerod_rpc::types::PathQuery`]). [`place`] works out, from the leaf's
//! index and the tree's size, which group of which layer each one is and
//! which member of it is the output's ancestor, and recomputes every hash
//! from the leaves up:
//!
//! 1. Each leaf is an output's key `O`, its key image generator `I` and its
//!    commitment `C`, as the x and y coordinates of each point on Wei25519,
//!    the Weierstrass form of Ed25519. An output from before Carrot may carry
//!    torsion, which the tree clears from `O` and `C` first, and derives its
//!    `I` with the older, biased hash of `O`. A Carrot output was checked for
//!    torsion when it was mined and uses the unbiased hash.
//! 2. A group of up to 38 leaves hashes, on the Selene curve, to one Selene
//!    point: its parent in layer 1.
//! 3. A group of up to 18 Selene points hashes, by their x coordinates, on
//!    Helios, to a Helios point in layer 2; 38 of those to a Selene point in
//!    layer 3; and so on, alternating, until one point is left: the root.
//!
//! The hash is a Pedersen-style commitment to the group, `init + sum(x_i *
//! G_i)` over fixed generators. These are `output_to_tuple`,
//! `output_tuple_to_leaf_tuple`, `calc_hashes_from_path` and `audit_path` in
//! monerod's `src/fcmp_pp/curve_trees.cpp`, with the curve arithmetic from
//! monero-oxide, which monerod itself links for it.
//!
//! The hash, its generators and the curves are monero-oxide's
//! (`fcmps::tree::hash_grow` and the FCMP++ generators), as are the
//! hash-to-point functions and the Wei25519 coordinates. What is here is the
//! leaf derivation and the walk up the path, which monero-oxide keeps inside
//! its prover.
//!
//! None of this is secret or says anything about who owns an output: the
//! tree is public and every node holds all of it.

use std::sync::LazyLock;

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use ec_divisors::DivisorCurve as _;
use helioselene::group::{GroupEncoding as _, ff::Field as _};
use helioselene::{Field25519, HeliosPoint, HelioseleneField, SelenePoint};
use monero_fcmp_plus_plus::fcmps::tree::hash_grow;
use monero_fcmp_plus_plus::{HELIOS_FCMP_GENERATORS, SELENE_FCMP_GENERATORS};
use monero_fcmp_plus_plus_generators::{HELIOS_HASH_INIT, SELENE_HASH_INIT};
use monerod_rpc::types::{
    HELIOS_CHUNK_WIDTH, LeafKind, PathLeaf, SELENE_CHUNK_WIDTH, TreePath, tree_layers,
};

/// The curve a layer's points are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Curve {
    /// The leaves: Ed25519 points, written as coordinates on Wei25519.
    Ed25519,
    Selene,
    Helios,
}

impl Curve {
    /// Layer 0 is the leaves; layer 1, their parents, is Selene; the curves
    /// alternate from there.
    #[must_use]
    pub const fn of_layer(layer: usize) -> Self {
        match layer {
            0 => Self::Ed25519,
            l if l % 2 == 1 => Self::Selene,
            _ => Self::Helios,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ed25519 => "Ed25519",
            Self::Selene => "Selene",
            Self::Helios => "Helios",
        }
    }
}

/// How many members of a layer share one parent: 38 for the leaves and each
/// Helios layer, whose parents are Selene nodes; 18 for each Selene layer.
#[must_use]
pub const fn group_width(layer: usize) -> u64 {
    match Curve::of_layer(layer) {
        Curve::Selene => HELIOS_CHUNK_WIDTH,
        Curve::Ed25519 | Curve::Helios => SELENE_CHUNK_WIDTH,
    }
}

/// One group of a path: the members of one layer that share a parent, the
/// output's ancestor among them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// 0 for the leaves, up to the root's layer.
    pub layer: usize,
    /// Members in the whole layer.
    pub layer_size: u64,
    /// The index in its layer of the group's first member.
    pub start: u64,
    /// How many members the group has: its layer's [`group_width`], or fewer
    /// for the last group of a layer.
    pub len: u64,
    /// The index in its layer of the output's ancestor, or of the output
    /// itself for the leaves.
    pub member: u64,
}

impl Group {
    /// The ancestor's place within the group.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.member - self.start
    }

    /// Which group of its layer this is, counted from 0.
    #[must_use]
    pub const fn index(&self) -> u64 {
        self.start / group_width(self.layer)
    }

    /// Groups in the whole layer.
    #[must_use]
    pub const fn groups_in_layer(&self) -> u64 {
        self.layer_size.div_ceil(group_width(self.layer))
    }
}

/// Where the groups of the path of leaf `leaf_idx` sit, in a tree of
/// `n_leaf_tuples` leaves: the leaves' group first, the root's last. Empty
/// for a leaf the tree does not have.
///
/// `CurveTrees::get_path_indexes` in `src/fcmp_pp/curve_trees.cpp`.
#[must_use]
pub fn path_groups(n_leaf_tuples: u64, leaf_idx: u64) -> Vec<Group> {
    if leaf_idx >= n_leaf_tuples {
        return Vec::new();
    }
    let sizes = std::iter::once(n_leaf_tuples).chain(tree_layers(n_leaf_tuples));
    let mut member = leaf_idx;
    sizes
        .enumerate()
        .map(|(layer, layer_size)| {
            let width = group_width(layer);
            let start = member - member % width;
            let group = Group {
                layer,
                layer_size,
                start,
                len: width.min(layer_size - start),
                member,
            };
            member /= width;
            group
        })
        .collect()
}

/// What recomputing a path's hashes found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathCheck {
    /// Every group hashes to its parent, up to the root.
    Holds,
    /// The group in `layer` (0 for the leaves) does not hash to the member
    /// of the layer above that the path says is its parent.
    Broken { layer: usize },
    /// A point in `layer` is not a valid point of its curve, or a leaf is of
    /// a type this build does not know, so the hash could not be computed.
    Unreadable { layer: usize },
    /// The path does not have the shape a path to this leaf in a tree of
    /// this size has.
    Misshapen,
}

/// An output's path, placed in the tree and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedPath {
    pub unified_id: u64,
    pub path: TreePath,
    /// One per layer, the leaves' first: `groups[k]` locates `path.leaves`
    /// for `k == 0` and `path.layers[k - 1]` above that.
    pub groups: Vec<Group>,
    pub check: PathCheck,
}

impl PlacedPath {
    /// The root the path leads to, as the daemon sent it. Whether the path's
    /// hashes lead there is [`Self::check`].
    #[must_use]
    pub fn root(&self) -> Option<&[u8; 32]> {
        self.path.layers.last().and_then(|l| l.first())
    }

    /// The members of `layer`'s group, as compressed points, or `None` for
    /// the leaves.
    #[must_use]
    pub fn members(&self, layer: usize) -> Option<&[[u8; 32]]> {
        self.path
            .layers
            .get(layer.checked_sub(1)?)
            .map(Vec::as_slice)
    }
}

/// Place and check the path the daemon sent for output `unified_id`, in a
/// tree of `n_leaf_tuples` leaves.
///
/// This costs a hash per layer and a point decompression and a hash to a
/// point per leaf in the output's group: a few milliseconds, all of it CPU.
#[must_use]
pub fn place(unified_id: u64, path: TreePath, n_leaf_tuples: u64) -> PlacedPath {
    Hashes::default().place(unified_id, path, n_leaf_tuples)
}

/// Place and check every path of one answer, `paths[i]` being the path of
/// `unified_ids[i]`.
///
/// A transaction's outputs usually share their groups, so each distinct
/// group is hashed once. Groups are told apart by their contents, not their
/// place, so two paths that disagree about a group are each checked.
#[must_use]
pub fn place_all(
    unified_ids: &[u64],
    paths: Vec<Option<TreePath>>,
    n_leaf_tuples: u64,
) -> Vec<Option<PlacedPath>> {
    let mut hashes = Hashes::default();
    paths
        .into_iter()
        .zip(unified_ids)
        .map(|(p, &id)| p.map(|p| hashes.place(id, p, n_leaf_tuples)))
        .collect()
}

/// The parents already computed, by the bytes of the group hashed.
#[derive(Default)]
struct Hashes(std::collections::HashMap<Vec<u8>, Option<[u8; 32]>>);

impl Hashes {
    fn place(&mut self, unified_id: u64, path: TreePath, n_leaf_tuples: u64) -> PlacedPath {
        let groups = path_groups(n_leaf_tuples, path.leaf_idx);
        let check = self.check(&path, &groups, unified_id);
        PlacedPath {
            unified_id,
            path,
            groups,
            check,
        }
    }

    fn memo(&mut self, key: Vec<u8>, hash: impl FnOnce() -> Option<[u8; 32]>) -> Option<[u8; 32]> {
        *self.0.entry(key).or_insert_with(hash)
    }

    fn check(&mut self, path: &TreePath, groups: &[Group], unified_id: u64) -> PathCheck {
        let Some((leaves, layers)) = groups.split_first() else {
            return PathCheck::Misshapen;
        };
        let shaped = layers.len() == path.layers.len()
            && usize::try_from(leaves.len).ok() == Some(path.leaves.len())
            && layers
                .iter()
                .zip(&path.layers)
                .all(|(g, members)| usize::try_from(g.len).ok() == Some(members.len()))
            && path
                .leaves
                .get(usize::try_from(leaves.offset()).unwrap_or(usize::MAX))
                .is_some_and(|l| l.unified_id == unified_id);
        if !shaped {
            return PathCheck::Misshapen;
        }

        // Layer 0 to 1: the leaves, hashed on Selene.
        let key = path
            .leaves
            .iter()
            .flat_map(|l| {
                let kind = match l.kind {
                    LeafKind::Legacy => 0,
                    LeafKind::Carrot => 1,
                    LeafKind::Other(b) => b,
                };
                std::iter::once(kind)
                    .chain(l.output_key)
                    .chain(l.commitment)
            })
            .collect();
        let Some(mut parent) = self.memo(key, || hash_leaves(&path.leaves)) else {
            return PathCheck::Unreadable { layer: 0 };
        };

        for (k, (group, members)) in layers.iter().zip(&path.layers).enumerate() {
            let layer = k + 1;
            let offset = usize::try_from(group.offset()).unwrap_or(usize::MAX);
            if members.get(offset) != Some(&parent) {
                return PathCheck::Broken { layer: layer - 1 };
            }
            if layer == path.layers.len() {
                break;
            }
            let mut key = vec![u8::try_from(layer).unwrap_or(u8::MAX)];
            key.extend(members.iter().flatten());
            match self.memo(key, || hash_layer(layer, members)) {
                Some(h) => parent = h,
                None => return PathCheck::Unreadable { layer },
            }
        }
        PathCheck::Holds
    }
}

/// A group of leaves' parent, in layer 1.
fn hash_leaves(leaves: &[PathLeaf]) -> Option<[u8; 32]> {
    let mut scalars = Vec::with_capacity(leaves.len() * 6);
    for leaf in leaves {
        scalars.extend(leaf_scalars(leaf)?);
    }
    Some(hash_selene(&scalars)?.to_bytes())
}

/// The parent of a group of `layer`'s members, in the layer above.
fn hash_layer(layer: usize, members: &[[u8; 32]]) -> Option<[u8; 32]> {
    match Curve::of_layer(layer) {
        Curve::Selene => {
            let xs = members.iter().map(selene_x).collect::<Option<Vec<_>>>()?;
            Some(hash_helios(&xs)?.to_bytes())
        }
        _ => {
            let xs = members.iter().map(helios_x).collect::<Option<Vec<_>>>()?;
            Some(hash_selene(&xs)?.to_bytes())
        }
    }
}

/// A leaf's six values: the Wei25519 x and y of `O`, `I` and `C`.
fn leaf_scalars(leaf: &PathLeaf) -> Option<[Field25519; 6]> {
    let key = CompressedEdwardsY(leaf.output_key).decompress()?;
    let commitment = CompressedEdwardsY(leaf.commitment).decompress()?;
    let (o, c, generator) = match leaf.kind {
        LeafKind::Legacy => (
            clear_torsion(key),
            clear_torsion(commitment),
            monero_ed25519::Point::biased_hash(leaf.output_key),
        ),
        LeafKind::Carrot => (
            key,
            commitment,
            monero_ed25519::Point::hash(leaf.output_key),
        ),
        LeafKind::Other(_) => return None,
    };
    let i = CompressedEdwardsY(generator.compress().to_bytes()).decompress()?;
    let wei =
        |p: EdwardsPoint| dalek_ff_group::EdwardsPoint::to_xy(dalek_ff_group::EdwardsPoint(p));
    let ((ox, oy), (ix, iy), (cx, cy)) = (wei(o)?, wei(i)?, wei(c)?);
    Some([ox, oy, ix, iy, cx, cy])
}

/// The point's prime-order part: `8 * (P / 8)`, as monerod's
/// `clear_torsion_vartime` computes it, which is `(8 * P) / 8` too.
fn clear_torsion(p: EdwardsPoint) -> EdwardsPoint {
    let inv_eight = curve25519_dalek::Scalar::from(8u8).invert();
    p.mul_by_cofactor() * inv_eight
}

/// A Selene point's x coordinate, which is a Helios scalar.
fn selene_x(bytes: &[u8; 32]) -> Option<HelioseleneField> {
    let p = Option::<SelenePoint>::from(SelenePoint::from_bytes(bytes))?;
    (p.to_bytes() == *bytes).then_some(())?;
    Some(SelenePoint::to_xy(p)?.0)
}

/// A Helios point's x coordinate, which is a Selene scalar.
fn helios_x(bytes: &[u8; 32]) -> Option<Field25519> {
    let p = Option::<HeliosPoint>::from(HeliosPoint::from_bytes(bytes))?;
    (p.to_bytes() == *bytes).then_some(())?;
    Some(HeliosPoint::to_xy(p)?.0)
}

/// A group's parent on Selene: `hash_grow` from an empty hash, over the
/// FCMP++ generators, as monerod's `get_new_parent` calls it.
fn hash_selene(children: &[Field25519]) -> Option<SelenePoint> {
    hash_grow(
        &SELENE_FCMP_GENERATORS.generators,
        *SELENE_HASH_INIT,
        0,
        Field25519::ZERO,
        children,
    )
}

/// A group's parent on Helios. See [`hash_selene`].
fn hash_helios(children: &[HelioseleneField]) -> Option<HeliosPoint> {
    hash_grow(
        &HELIOS_FCMP_GENERATORS.generators,
        *HELIOS_HASH_INIT,
        0,
        HelioseleneField::ZERO,
        children,
    )
}

/// Load the generators the tree's hashes use.
///
/// monero-oxide builds them into the binary and decodes them on first use,
/// which takes a second or two: all of the FCMP++ proof's generators are
/// decoded at once. Calling this at startup, off the request threads, keeps
/// that cost from landing on the first page to check a path.
pub fn load_generators() {
    LazyLock::force(&SELENE_FCMP_GENERATORS);
    LazyLock::force(&HELIOS_FCMP_GENERATORS);
    LazyLock::force(&SELENE_HASH_INIT);
    LazyLock::force(&HELIOS_HASH_INIT);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn a_leaf_deep_in_a_mainnet_sized_tree_sits_in_one_group_per_layer() {
        let groups = path_groups(152_318_407, 100_000_000);
        let sizes: Vec<u64> = groups.iter().map(|g| g.layer_size).collect();
        assert_eq!(sizes, [152_318_407, 4_008_380, 222_688, 5_861, 326, 9, 1]);
        // The ancestor at each layer is the one below divided by the width
        // of the groups below it.
        let members: Vec<u64> = groups.iter().map(|g| g.member).collect();
        assert_eq!(members, [100_000_000, 2_631_578, 146_198, 3_847, 213, 5, 0]);
        let lens: Vec<u64> = groups.iter().map(|g| g.len).collect();
        assert_eq!(lens, [38, 18, 38, 18, 38, 9, 1]);
        assert_eq!(groups[1].start, 2_631_564);
        assert_eq!(groups[1].offset(), 14);
        assert_eq!(groups[1].index(), 146_198);
        assert_eq!(groups[1].groups_in_layer(), 222_688);
    }

    #[test]
    fn the_last_leaf_sits_in_the_short_last_group_of_each_layer() {
        let groups = path_groups(760, 759);
        let lens: Vec<u64> = groups.iter().map(|g| g.len).collect();
        // 760 = 20 * 38, so its group of leaves is full; the 20 parents are a
        // group of 18 and one of 2; above them, one group of 2, then the root.
        assert_eq!(lens, [38, 2, 2, 1]);
        assert!(path_groups(760, 760).is_empty());
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/fcmp/paths")
            .join(name)
    }

    fn paths(name: &str, as_of_block: u64, ids: &[u64]) -> monerod_rpc::types::TreePaths {
        let q = monerod_rpc::types::PathQuery::as_of_block(as_of_block, ids).unwrap();
        let bytes = std::fs::read(fixture(name)).unwrap();
        let root =
            monerod_rpc::epee::read_root(&bytes, monerod_rpc::types::PathQuery::WANTED).unwrap();
        q.answer(&root).unwrap()
    }

    /// The root a block records, as bytes.
    fn block_root(name: &str) -> [u8; 32] {
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture(name)).unwrap()).unwrap();
        let block: serde_json::Value =
            serde_json::from_str(v["result"]["json"].as_str().unwrap()).unwrap();
        let hex = block["fcmp_pp_tree_root"].as_str().unwrap();
        let mut out = [0u8; 32];
        crate::hex::decode_to_slice(hex, &mut out).unwrap();
        out
    }

    /// Recomputed from the leaves up, the paths monerod sent reach the root
    /// monerod recorded, eight blocks behind the tree they were taken from.
    #[test]
    fn real_paths_hash_up_to_the_root_their_block_records() {
        for (bin, as_of, ids, block) in [
            (
                "get_path_by_unified_id_tip.bin",
                811,
                &[802u64, 803, 804, 805][..],
                "get_block_root_tip.json",
            ),
            (
                "get_path_by_unified_id_later.bin",
                814,
                &[802, 803, 804, 805],
                "get_block_root_later.json",
            ),
            (
                "get_path_by_unified_id_old.bin",
                814,
                &[10, 60],
                "get_block_root_later.json",
            ),
        ] {
            let answer = paths(bin, as_of, ids);
            let expected = block_root(block);
            for (path, &id) in answer.paths.into_iter().zip(ids) {
                let placed = place(id, path.unwrap(), answer.n_leaf_tuples);
                assert_eq!(placed.check, PathCheck::Holds, "{bin} output {id}");
                assert_eq!(placed.root(), Some(&expected), "{bin} output {id}");
            }
        }
    }

    /// Output 0, the genesis coinbase, predates Carrot: its leaf takes the
    /// legacy derivation, torsion clearing and the biased hash, and the path
    /// holding it still reaches the root.
    #[test]
    fn a_legacy_leaf_hashes_the_way_monerod_hashes_it() {
        let answer = paths("get_path_by_unified_id_old.bin", 814, &[10, 60]);
        let path = answer.paths[0].clone().unwrap();
        assert_eq!(path.leaves[0].kind, LeafKind::Legacy);
        assert_eq!(
            place(10, path, answer.n_leaf_tuples).check,
            PathCheck::Holds
        );
    }

    /// Any one byte changed anywhere in a path is caught, at the layer it is
    /// in.
    #[test]
    fn a_tampered_path_is_caught_at_its_layer() {
        let answer = paths("get_path_by_unified_id_old.bin", 814, &[10, 60]);
        let good = answer.paths[0].clone().unwrap();
        let n = answer.n_leaf_tuples;

        // A different leaf in the output's group: the leaves no longer hash
        // to their parent.
        let mut p = good.clone();
        p.leaves.swap(3, 4);
        assert_eq!(place(10, p, n).check, PathCheck::Broken { layer: 0 });

        // A sibling in layer 1 swapped for the ancestor's neighbour's value:
        // layer 1 no longer hashes to layer 2.
        let mut p = good.clone();
        p.layers[0].swap(5, 6);
        assert_eq!(place(10, p, n).check, PathCheck::Broken { layer: 1 });

        // A root the path does not lead to.
        let mut p = good.clone();
        p.layers[2][0] = p.layers[1][1];
        assert_eq!(place(10, p, n).check, PathCheck::Broken { layer: 2 });

        // Not a point at all.
        let mut p = good.clone();
        p.layers[1][1] = [0xff; 32];
        assert_eq!(place(10, p, n).check, PathCheck::Unreadable { layer: 2 });

        // A path for a different leaf than the one asked about.
        assert_eq!(place(11, good.clone(), n).check, PathCheck::Misshapen);
        // Or for a tree of another size.
        assert_eq!(place(10, good, n + 38 * 18).check, PathCheck::Misshapen);
    }

    /// One answer's paths, placed together, come out as they do placed one
    /// at a time, and a group two paths disagree about is checked for each.
    #[test]
    fn paths_placed_together_are_each_checked() {
        let answer = paths("get_path_by_unified_id_tip.bin", 811, &[802, 803, 804, 805]);
        let ids = [802, 803, 804, 805];
        let together = place_all(&ids, answer.paths.clone(), answer.n_leaf_tuples);
        for ((t, p), &id) in together.iter().zip(&answer.paths).zip(&ids) {
            let alone = place(id, p.clone().unwrap(), answer.n_leaf_tuples);
            assert_eq!(t.as_ref(), Some(&alone));
            assert_eq!(alone.check, PathCheck::Holds);
        }

        let mut tampered = answer.paths.clone();
        let last = tampered[3].as_mut().unwrap();
        last.layers[0].swap(0, 1);
        let together = place_all(&ids, tampered, answer.n_leaf_tuples);
        assert_eq!(together[0].as_ref().unwrap().check, PathCheck::Holds);
        assert_ne!(together[3].as_ref().unwrap().check, PathCheck::Holds);
    }

    #[test]
    fn layers_alternate_curves_from_selene() {
        let curves: Vec<&str> = (0..5).map(|l| Curve::of_layer(l).name()).collect();
        assert_eq!(curves, ["Ed25519", "Selene", "Helios", "Selene", "Helios"]);
        let widths: Vec<u64> = (0..5).map(group_width).collect();
        assert_eq!(widths, [38, 18, 38, 18, 38]);
    }
}
