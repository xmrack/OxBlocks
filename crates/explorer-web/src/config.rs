//! Command line configuration.

use std::net::SocketAddr;
use std::time::Duration;

use clap::{Parser, ValueEnum};

/// Which palette the stylesheet carries.
///
/// A per-reader toggle would need a cookie or a script, and this explorer
/// serves neither, so the choice is the operator's: `auto` hands it back to
/// the reader's browser, the other two pin it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Theme {
    /// Follow the reader's `prefers-color-scheme`.
    #[default]
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "oxblocks",
    version,
    about = "A memory-safe Monero block explorer for monerod",
    long_about = None
)]
pub struct Config {
    /// monerod RPC URL. Must be an UNRESTRICTED daemon.
    ///
    /// Under `--restricted-rpc` monerod blocks /get_transaction_pool,
    /// get_alternate_chains and get_alt_blocks_hashes, which removes the
    /// mempool and alt-block pages.
    #[arg(
        long,
        env = "OXBLOCKS_DAEMON_URL",
        default_value = "http://127.0.0.1:18081"
    )]
    pub daemon_url: String,

    /// Address to listen on.
    #[arg(long, env = "OXBLOCKS_BIND", default_value = "127.0.0.1:8081")]
    pub bind: SocketAddr,

    /// Seconds to wait on a single monerod RPC call.
    #[arg(long, env = "OXBLOCKS_RPC_TIMEOUT", default_value_t = 30)]
    pub rpc_timeout_secs: u64,

    /// Seconds before an inbound HTTP request is abandoned.
    ///
    /// Held below the RPC timeout on purpose: a request that has already
    /// outlived its own deadline should not keep an upstream call alive.
    #[arg(long, env = "OXBLOCKS_REQUEST_TIMEOUT", default_value_t = 25)]
    pub request_timeout_secs: u64,

    /// Maximum number of requests processed concurrently.
    ///
    /// This is the backpressure valve. Every request costs upstream RPC calls,
    /// so an unbounded server would turn a traffic spike into a self-inflicted
    /// denial of service against its own daemon.
    #[arg(long, env = "OXBLOCKS_MAX_CONCURRENT", default_value_t = 128)]
    pub max_concurrent: usize,

    /// Maximum RPC calls in flight against monerod at once.
    ///
    /// The other half of `--max-concurrent`, and the one that protects the
    /// daemon rather than this process. monerod answers RPC on a bounded
    /// thread pool shared with its peer-to-peer duties, so a burst of requests
    /// here degrades the node itself. Requests that cannot get a slot queue in
    /// front of the daemon rather than stampeding it, and the inbound request
    /// timeout eventually sheds them.
    #[arg(
        long,
        env = "OXBLOCKS_MAX_INFLIGHT_RPC",
        default_value_t = explorer_core::DEFAULT_MAX_INFLIGHT_RPC
    )]
    pub max_inflight_rpc: usize,

    /// Maximum accepted request body size, in bytes.
    ///
    /// Every route is a GET. This exists to make that explicit rather than
    /// relying on it.
    #[arg(long, env = "OXBLOCKS_MAX_BODY", default_value_t = 8 * 1024)]
    pub max_body_bytes: usize,

    /// Colour scheme: auto, light or dark.
    #[arg(long, env = "OXBLOCKS_THEME", value_enum, default_value = "auto")]
    pub theme: Theme,

    /// Log filter, e.g. "info", "oxblocks=debug,tower_http=debug".
    #[arg(long, env = "OXBLOCKS_LOG", default_value = "info")]
    pub log: String,
}

impl Config {
    pub fn rpc_timeout(&self) -> Duration {
        Duration::from_secs(self.rpc_timeout_secs)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }
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
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Config::command().debug_assert();
    }

    #[test]
    fn defaults_point_at_a_loopback_unrestricted_daemon() {
        let c = Config::parse_from(["oxblocks"]);
        assert_eq!(c.daemon_url, "http://127.0.0.1:18081");
        assert_eq!(c.bind.to_string(), "127.0.0.1:8081");
    }

    /// The request deadline must stay under the upstream deadline, or a client
    /// that has already given up still pins an RPC call open.
    #[test]
    fn request_timeout_is_shorter_than_rpc_timeout_by_default() {
        let c = Config::parse_from(["oxblocks"]);
        assert!(c.request_timeout() < c.rpc_timeout());
    }

    /// The daemon-side ceiling has to be reachable from the command line: it
    /// is the knob an operator turns when their node is shared with something
    /// else, and it was previously settable only from a test.
    #[test]
    fn the_inflight_rpc_ceiling_is_configurable_and_defaults_to_the_library_value() {
        let c = Config::parse_from(["oxblocks"]);
        assert_eq!(c.max_inflight_rpc, explorer_core::DEFAULT_MAX_INFLIGHT_RPC);
        let c = Config::parse_from(["oxblocks", "--max-inflight-rpc", "4"]);
        assert_eq!(c.max_inflight_rpc, 4);
    }

    /// `auto` is the only default that respects a reader's own setting; the
    /// other two exist for an operator who wants one look regardless.
    #[test]
    fn the_theme_defaults_to_the_readers_own_preference() {
        assert_eq!(Config::parse_from(["oxblocks"]).theme, Theme::Auto);
        assert_eq!(
            Config::parse_from(["oxblocks", "--theme", "light"]).theme,
            Theme::Light
        );
        assert_eq!(
            Config::parse_from(["oxblocks", "--theme", "dark"]).theme,
            Theme::Dark
        );
        assert!(Config::try_parse_from(["oxblocks", "--theme", "sepia"]).is_err());
    }

    #[test]
    fn a_bad_bind_address_is_rejected_rather_than_defaulted() {
        assert!(Config::try_parse_from(["oxblocks", "--bind", "not-an-address"]).is_err());
    }
}
