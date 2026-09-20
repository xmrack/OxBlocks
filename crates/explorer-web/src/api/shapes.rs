//! Response shapes, field-for-field compatible with the C++ explorer.
//!
//! **Every struct here declares its fields in alphabetical order.** nlohmann
//! stores objects in a `std::map`, so upstream emits keys byte-ascending and
//! recursively; serde emits them in *declaration* order. Declaring them sorted
//! is how the two agree without routing everything through
//! `serde_json::Value`.
//!
//! Two things hold the property up, and only one of them is load-bearing today.
//! The declaration order is the belt: `declaration_order_is_alphabetical` below
//! checks it by serialising each struct **directly**, which streams its fields
//! in the order they are written. The braces is that `serde_json::Map` is a
//! `BTreeMap` unless the `preserve_order` feature is on, and the real response
//! path goes through `serde_json::to_value`, which therefore sorts whatever it
//! is given. That feature could be switched on by a transitive dependency
//! without anyone here noticing, at which point the belt is all that is left --
//! so `no_preserve_order` pins it.
//!
//! Both live in this file's `tests` module, which is where they were missing
//! from: this comment claimed they existed long before they did.

use explorer_core::fmt::timestamp_utc;
use explorer_core::{Hash32, ResolvedInput, TxFacts};
use monerod_rpc::types::{PoolTxInfo, TxEntry, TxJson, TxOutTarget};
use serde::Serialize;

/// One ring member.
///
/// The height field is named `block_no`, not `height`. That is upstream's
/// name and the one clients index on, so it is not ours to tidy.
#[derive(Debug, Clone, Serialize)]
pub struct ApiMixin {
    pub block_no: u64,
    pub public_key: String,
    pub tx_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiInput {
    pub amount: u64,
    pub key_image: String,
    /// `null`, not `[]`, when the ring could not be resolved.
    ///
    /// Upstream initialises this as `json {}` — which is null — and only
    /// becomes an array on first `push_back`. An input whose very first ring
    /// member fails to resolve therefore serialises as `"mixins":null`, and a
    /// plain `Vec` would emit `[]` and diverge.
    pub mixins: Option<Vec<ApiMixin>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiOutput {
    pub amount: u64,
    pub public_key: String,
}

/// `get_tx_json` — the shared 12-key transaction object that appears inside
/// `/api/block`, `/api/transactions` and `/api/mempool`.
#[derive(Debug, Clone, Serialize)]
pub struct TxSummary {
    pub coinbase: bool,
    pub extra: String,
    pub mixin: u64,
    pub payment_id: String,
    pub payment_id8: String,
    pub rct_type: u8,
    pub tx_fee: u64,
    pub tx_hash: String,
    pub tx_size: u64,
    pub tx_version: u64,
    /// devel emits this; master does not. Kept because a transaction that
    /// cannot be spent until a given height or time is a fact a reader wants,
    /// and it costs one field.
    pub unlock_time: u64,
    pub xmr_inputs: u64,
    pub xmr_outputs: u64,
}

/// `/api/transaction` — the 13 shared keys plus seven more.
#[derive(Debug, Clone, Serialize)]
pub struct TxDetail {
    pub block_height: u64,
    pub coinbase: bool,
    pub confirmations: u64,
    pub current_height: u64,
    pub extra: String,
    /// `null` for a coinbase transaction, confirmed against a live upstream
    /// capture. Upstream declares `json inputs;` and never enters the loop for
    /// a lone `txin_gen`, so it stays null rather than becoming `[]`.
    pub inputs: Option<Vec<ApiInput>>,
    pub mixin: u64,
    pub outputs: Vec<ApiOutput>,
    pub payment_id: String,
    pub payment_id8: String,
    pub rct_type: u8,
    pub timestamp: u64,
    pub timestamp_utc: String,
    pub tx_fee: u64,
    pub tx_hash: String,
    pub tx_size: u64,
    pub tx_version: u64,
    pub unlock_time: u64,
    pub xmr_inputs: u64,
    pub xmr_outputs: u64,
}

/// `/api/block`.
#[derive(Debug, Clone, Serialize)]
pub struct BlockDetail {
    pub block_height: u64,
    pub current_height: u64,
    pub hash: String,
    /// Integer here. The *same* value is a JSON float in `/api/transactions`,
    /// because upstream holds it in a `uint64_t` in one builder and a `double`
    /// in the other. Confirmed on one block: 95511 versus 95511.0.
    pub size: u64,
    pub timestamp: u64,
    pub timestamp_utc: String,
    pub txs: Vec<TxSummary>,
}

impl TxSummary {
    pub fn build(entry: &TxEntry, tx: &TxJson) -> Self {
        Self::from_facts(&entry.tx_hash, &TxFacts::from_entry(entry, tx))
    }

    /// The same shape for a transaction that is still in the pool.
    ///
    /// A pool entry has no `TxEntry`: there is no block, no confirmations, and
    /// the size is the pool's own `blob_size` rather than a reassembled hex
    /// blob. The fee is stated by the pool too, so it is taken rather than
    /// recomputed.
    pub fn build_pool(info: &PoolTxInfo, tx: &TxJson) -> Self {
        Self::from_facts(&info.id_hash, &TxFacts::from_pool(info, tx))
    }

    fn from_facts(hash: &str, f: &TxFacts) -> Self {
        Self {
            coinbase: f.coinbase,
            extra: f.extra_hex(),
            mixin: f.ring_size as u64,
            payment_id: f.payment_id_hex(),
            payment_id8: f.payment_id8_hex(),
            rct_type: f.rct_type,
            tx_fee: f.fee,
            tx_hash: hash.to_lowercase(),
            tx_size: f.size,
            tx_version: f.version,
            unlock_time: f.unlock_time,
            xmr_inputs: f.xmr_inputs,
            xmr_outputs: f.xmr_outputs,
        }
    }
}

/// Where a transaction sits, which is all three of the fields that answer
/// "when". A transaction in the pool is in no block: height zero,
/// confirmations zero, and the time it carries is the time it arrived --
/// monerod puts that in `received_timestamp`, and reading `block_timestamp`
/// regardless reports every unconfirmed transaction as dated 1970.
struct Placement {
    block_height: u64,
    timestamp: u64,
    confirmations: u64,
}

impl Placement {
    fn of(entry: &TxEntry, current_height: u64) -> Self {
        if entry.in_pool {
            return Self::pool(entry.received_timestamp);
        }
        Self {
            block_height: entry.block_height,
            timestamp: entry.block_timestamp,
            confirmations: current_height.saturating_sub(entry.block_height),
        }
    }

    const fn pool(received: u64) -> Self {
        Self {
            block_height: 0,
            timestamp: received,
            confirmations: 0,
        }
    }
}

impl TxDetail {
    pub fn build(
        entry: &TxEntry,
        tx: &TxJson,
        rings: &[ResolvedInput],
        current_height: u64,
    ) -> Self {
        Self::assemble(
            &entry.tx_hash,
            &TxFacts::from_entry(entry, tx),
            tx,
            rings,
            &Placement::of(entry, current_height),
            current_height,
        )
    }

    /// The same shape for a transaction still in the pool, built from the pool
    /// listing itself.
    ///
    /// `/get_transaction_pool` already carries each transaction's JSON, so
    /// asking `/get_transactions` for the same hashes is a round trip spent
    /// re-fetching what the daemon has already sent. A pool entry states its
    /// own size and fee, which is why `TxFacts` has a second constructor.
    pub fn build_pool(
        info: &PoolTxInfo,
        tx: &TxJson,
        rings: &[ResolvedInput],
        current_height: u64,
    ) -> Self {
        Self::assemble(
            &info.id_hash,
            &TxFacts::from_pool(info, tx),
            tx,
            rings,
            &Placement::pool(info.receive_time),
            current_height,
        )
    }

    fn assemble(
        hash: &str,
        f: &TxFacts,
        tx: &TxJson,
        rings: &[ResolvedInput],
        at: &Placement,
        current_height: u64,
    ) -> Self {
        let inputs = if f.coinbase {
            None
        } else {
            Some(
                rings
                    .iter()
                    .map(|r| ApiInput {
                        amount: r.amount,
                        key_image: r.key_image.to_hex(),
                        mixins: if r.ring.is_empty() && r.ring_unavailable {
                            None
                        } else {
                            Some(
                                r.ring
                                    .iter()
                                    .map(|m| ApiMixin {
                                        block_no: m.block_height,
                                        public_key: m.public_key.to_hex(),
                                        tx_hash: m.tx_hash.to_hex(),
                                    })
                                    .collect(),
                            )
                        },
                    })
                    .collect(),
            )
        };

        let outputs = tx
            .vout
            .iter()
            .map(|o| ApiOutput {
                amount: o.amount,
                public_key: match &o.target {
                    TxOutTarget::Key(k) => k.clone(),
                    TxOutTarget::TaggedKey(t) => t.key.clone(),
                    other => {
                        // Legacy script outputs exist only in the pre-v1 era
                        // and carry no one-time key. Render empty rather than
                        // inventing one.
                        let _ = other;
                        String::new()
                    }
                },
            })
            .collect();

        Self {
            block_height: at.block_height,
            coinbase: f.coinbase,
            confirmations: at.confirmations,
            current_height,
            extra: f.extra_hex(),
            inputs,
            mixin: f.ring_size as u64,
            outputs,
            payment_id: f.payment_id_hex(),
            payment_id8: f.payment_id8_hex(),
            rct_type: f.rct_type,
            timestamp: at.timestamp,
            timestamp_utc: timestamp_utc(at.timestamp),
            tx_fee: f.fee,
            tx_hash: hash.to_lowercase(),
            tx_size: f.size,
            tx_version: f.version,
            unlock_time: f.unlock_time,
            xmr_inputs: f.xmr_inputs,
            xmr_outputs: f.xmr_outputs,
        }
    }
}

/// Hash rendering for a value that came off the wire as a string.
///
/// monerod returns lowercase hex, but a user may have *asked* in uppercase and
/// upstream echoes the parsed value, so normalise rather than pass through.
#[must_use]
pub fn normalise_hash(raw: &str) -> String {
    raw.parse::<Hash32>()
        .map_or_else(|_| raw.to_lowercase(), |h| h.to_hex())
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

    /// The key order a struct *emits* at its own level, before anything
    /// re-sorts it.
    ///
    /// Two traps, both of which this walked into on the first attempt:
    ///
    /// * going through `to_value` first would sort the keys and make every
    ///   assertion below vacuously true, so this serialises the struct
    ///   directly, which streams its fields in declaration order;
    /// * a nested object's keys are *not* part of its parent's ordering.
    ///   Flattening them into one list compares `block_no` against
    ///   `confirmations` and fails on structs that are perfectly ordered. Only
    ///   keys at depth 1 are collected; the nested shapes are asserted in their
    ///   own right.
    fn declared_keys<T: Serialize>(value: &T) -> Vec<String> {
        let rendered = serde_json::to_string(value).expect("serialises");
        let mut keys = Vec::new();
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut current = String::new();
        let mut chars = rendered.chars().peekable();

        while let Some(c) = chars.next() {
            if in_string {
                if escaped {
                    escaped = false;
                    current.push(c);
                } else if c == '\\' {
                    escaped = true;
                    current.push(c);
                } else if c == '"' {
                    in_string = false;
                    // A string at depth 1 followed by ':' is a key of the
                    // object under test; anything else is a value.
                    if depth == 1 && chars.peek() == Some(&':') {
                        keys.push(current.clone());
                    }
                } else {
                    current.push(c);
                }
                continue;
            }
            match c {
                '"' => {
                    in_string = true;
                    current.clear();
                }
                '{' | '[' => depth += 1,
                '}' | ']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        keys
    }

    fn assert_sorted(keys: &[String], what: &str) {
        let mut expected = keys.to_vec();
        expected.sort();
        assert_eq!(
            keys, expected,
            "{what} declares its fields out of alphabetical order, so it would \
             emit keys in a different order from upstream if serde_json ever \
             stopped sorting them"
        );
    }

    fn mixin() -> ApiMixin {
        ApiMixin {
            block_no: 1,
            public_key: String::new(),
            tx_hash: String::new(),
        }
    }

    fn summary() -> TxSummary {
        TxSummary {
            coinbase: false,
            extra: String::new(),
            mixin: 16,
            payment_id: String::new(),
            payment_id8: String::new(),
            rct_type: 0,
            tx_fee: 0,
            tx_hash: String::new(),
            tx_size: 0,
            tx_version: 1,
            unlock_time: 0,
            xmr_inputs: 0,
            xmr_outputs: 0,
        }
    }

    fn detail() -> TxDetail {
        TxDetail {
            block_height: 0,
            coinbase: false,
            confirmations: 0,
            current_height: 0,
            extra: String::new(),
            inputs: Some(vec![ApiInput {
                amount: 0,
                key_image: String::new(),
                mixins: Some(vec![mixin()]),
            }]),
            mixin: 16,
            outputs: vec![ApiOutput {
                amount: 0,
                public_key: String::new(),
            }],
            payment_id: String::new(),
            payment_id8: String::new(),
            rct_type: 0,
            timestamp: 0,
            timestamp_utc: String::new(),
            tx_fee: 0,
            tx_hash: String::new(),
            tx_size: 0,
            tx_version: 2,
            unlock_time: 0,
            xmr_inputs: 0,
            xmr_outputs: 0,
        }
    }

    /// Upstream emits keys byte-ascending because nlohmann stores objects in a
    /// `std::map`. Every struct here has to be declared in that order to agree
    /// with it without routing through a sorting container.
    ///
    /// The module comment has claimed since it was written that this is
    /// "checked mechanically". It was not: there was no test module here at
    /// all until this one.
    #[test]
    fn declaration_order_is_alphabetical() {
        assert_sorted(&declared_keys(&mixin()), "ApiMixin");
        assert_sorted(
            &declared_keys(&ApiInput {
                amount: 0,
                key_image: String::new(),
                mixins: None,
            }),
            "ApiInput",
        );
        assert_sorted(
            &declared_keys(&ApiOutput {
                amount: 0,
                public_key: String::new(),
            }),
            "ApiOutput",
        );
        assert_sorted(&declared_keys(&summary()), "TxSummary");
        assert_sorted(&declared_keys(&detail()), "TxDetail");
        assert_sorted(
            &declared_keys(&BlockDetail {
                block_height: 0,
                current_height: 0,
                hash: String::new(),
                size: 0,
                timestamp: 0,
                timestamp_utc: String::new(),
                txs: vec![summary()],
            }),
            "BlockDetail",
        );
    }

    /// The helper itself, because an ordering test built on a broken scanner
    /// would pass for the wrong reason.
    ///
    /// Upstream sorts recursively, so every nested object has to be ordered
    /// too -- but it is ordered *against its own siblings*, not against its
    /// parent's fields. `ApiMixin` and `ApiOutput` are asserted separately
    /// above for that reason, and must not leak into `TxDetail`'s list here.
    #[test]
    fn the_scanner_reads_one_level_and_does_not_mistake_values_for_keys() {
        let keys = declared_keys(&detail());
        assert!(
            !keys.iter().any(|k| k == "block_no"),
            "a nested ring-member key was counted as one of TxDetail's own"
        );
        assert!(
            keys.iter().any(|k| k == "inputs") && keys.iter().any(|k| k == "outputs"),
            "the containing fields themselves were missed"
        );
        assert_eq!(keys.len(), 20, "TxDetail has 20 fields of its own");

        // A value that happens to be a string must not be read as a key.
        let probe = ApiOutput {
            amount: 0,
            public_key: "not_a_key".to_owned(),
        };
        assert_eq!(declared_keys(&probe), vec!["amount", "public_key"]);
    }

    /// What the ordering actually rests on today.
    ///
    /// `serde_json::Map` is a `BTreeMap` unless the `preserve_order` feature is
    /// enabled, and the response path runs every payload through
    /// `serde_json::to_value`, so the map sorts the keys whatever order the
    /// struct declared them in. Cargo unifies features across the whole graph,
    /// so a dependency added three levels away could switch that on; the
    /// ordering would then silently become declaration order.
    #[test]
    fn no_preserve_order() {
        #[derive(Serialize)]
        struct OutOfOrder {
            zebra: u64,
            apple: u64,
        }
        let sorted_by_the_map =
            serde_json::to_value(OutOfOrder { zebra: 1, apple: 2 }).expect("serialises");
        assert_eq!(
            serde_json::to_string(&sorted_by_the_map).expect("serialises"),
            r#"{"apple":2,"zebra":1}"#,
            "serde_json is no longer sorting object keys, which means the \
             `preserve_order` feature has been switched on somewhere in the \
             dependency graph. Upstream emits keys byte-ascending, so every \
             response struct's declaration order is now load-bearing."
        );
    }
}
