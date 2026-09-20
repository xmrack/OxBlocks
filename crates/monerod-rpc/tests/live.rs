//! Tests that require a real monerod. Ignored by default so `cargo test` stays
//! hermetic; the fixture-based tests cover the same ground in CI.
//!
//! ```text
//! OXBLOCKS_TEST_RPC=http://127.0.0.1:28081 cargo test -p monerod-rpc -- --ignored --nocapture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use monerod_rpc::{Client, RpcError};

fn node() -> Option<Client> {
    let url = std::env::var("OXBLOCKS_TEST_RPC").ok()?;
    Client::new(url).ok()
}

macro_rules! require_node {
    () => {
        match node() {
            Some(c) => c,
            None => {
                eprintln!("skipping: set OXBLOCKS_TEST_RPC to a monerod URL");
                return;
            }
        }
    };
}

#[tokio::test]
#[ignore = "requires a running monerod"]
async fn json_rpc_round_trip() {
    let node = require_node!();
    let info: serde_json::Value = node
        .json_rpc("get_info", None::<()>)
        .await
        .expect("get_info should succeed");

    let height = info["height"].as_u64().expect("height is a number");
    assert!(height > 0, "a live node reports a non-zero height");
    println!("height={height} nettype={}", info["nettype"]);
}

#[tokio::test]
#[ignore = "requires a running monerod"]
async fn bare_endpoint_round_trip() {
    let node = require_node!();
    let pool: serde_json::Value = node
        .endpoint("get_transaction_pool", &serde_json::json!({}))
        .await
        .expect("get_transaction_pool should succeed on an unrestricted node");
    assert!(pool.get("status").is_some());
}

/// The envelope-level failure path: monerod answers HTTP 200 with an `error`
/// member, which must not be mistaken for success.
#[tokio::test]
#[ignore = "requires a running monerod"]
async fn json_rpc_error_is_surfaced_not_swallowed() {
    let node = require_node!();
    let result: Result<serde_json::Value, _> = node
        .json_rpc("get_block", Some(serde_json::json!({ "height": u64::MAX })))
        .await;

    match result {
        Err(RpcError::JsonRpc { code, message, .. }) => {
            println!("monerod rejected as expected: code={code} message={message}");
        }
        Err(other) => panic!("expected a JsonRpc error, got {other}"),
        Ok(_) => panic!("monerod should not return a block at height u64::MAX"),
    }
}

/// An unknown method must surface as an error rather than decoding into a
/// default-shaped success value.
#[tokio::test]
#[ignore = "requires a running monerod"]
async fn unknown_method_is_an_error() {
    let node = require_node!();
    let result: Result<serde_json::Value, _> =
        node.json_rpc("no_such_method_exists", None::<()>).await;
    assert!(result.is_err(), "unknown methods must not succeed");
}
