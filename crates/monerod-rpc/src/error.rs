//! Errors produced while talking to monerod.

/// The `status` string that monerod puts on most non-JSON-RPC responses.
///
/// monerod does not use HTTP status codes to signal application failure: a
/// perfectly successful HTTP 200 can carry `"status": "Failed"`. Anything that
/// only checks the HTTP status silently treats failures as success, so every
/// response goes through here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// The daemon is still syncing and declined to answer.
    Busy,
    Failed,
    NotMining,
    Other(String),
}

impl Status {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "OK" => Self::Ok,
            "BUSY" => Self::Busy,
            "Failed" => Self::Failed,
            "NOT MINING" => Self::NotMining,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => f.write_str("OK"),
            Self::Busy => f.write_str("BUSY"),
            Self::Failed => f.write_str("Failed"),
            Self::NotMining => f.write_str("NOT MINING"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("could not reach monerod: {0}")]
    Transport(#[from] reqwest::Error),

    /// monerod answered the JSON-RPC envelope with an `error` member.
    #[error("monerod rejected {method}: {message} (code {code})")]
    JsonRpc {
        method: &'static str,
        code: i64,
        message: String,
    },

    /// A non-JSON-RPC endpoint answered with a non-OK `status`.
    #[error("monerod answered {endpoint} with status {status}")]
    Status {
        endpoint: &'static str,
        status: Status,
    },

    /// monerod answered with a non-success HTTP status. Distinct from
    /// [`RpcError::Status`]: this is the transport refusing, not the daemon
    /// reporting. A 401 here means `--rpc-login` is set and we have no credentials.
    #[error("monerod answered {context} with HTTP {status}: {body}")]
    Http {
        context: &'static str,
        status: u16,
        body: String,
    },

    #[error("could not decode monerod's response to {context}: {source}")]
    Decode {
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },

    /// monerod answered, but the payload did not contain what the call promises.
    #[error("monerod's response to {context} had no {field} field")]
    Missing {
        context: &'static str,
        field: &'static str,
    },

    #[error("{0} is not a usable monerod URL: {1}")]
    BadUrl(String, String),
}

impl RpcError {
    /// Whether retrying the same call later might succeed.
    ///
    /// `BUSY` means the daemon is syncing, which is the normal state of a node
    /// during initial sync, so callers generally want to back off rather than
    /// surface a hard failure.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Status { status, .. } => matches!(status, Status::Busy),
            Self::Transport(e) => e.is_timeout() || e.is_connect(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_status_strings_monerod_actually_sends() {
        assert_eq!(Status::parse("OK"), Status::Ok);
        assert_eq!(Status::parse("BUSY"), Status::Busy);
        assert_eq!(Status::parse("Failed"), Status::Failed);
        assert_eq!(Status::parse("NOT MINING"), Status::NotMining);
        assert!(Status::parse("OK").is_ok());
        assert!(!Status::parse("Failed").is_ok());
    }

    #[test]
    fn unknown_status_is_preserved_rather_than_coerced_to_ok() {
        // A future monerod status we do not know about must never read as success.
        let s = Status::parse("SOMETHING_NEW");
        assert!(!s.is_ok());
        assert_eq!(s.to_string(), "SOMETHING_NEW");
    }

    #[test]
    fn busy_is_transient_but_failed_is_not() {
        let busy = RpcError::Status {
            endpoint: "/get_transactions",
            status: Status::Busy,
        };
        let failed = RpcError::Status {
            endpoint: "/get_transactions",
            status: Status::Failed,
        };
        assert!(busy.is_transient());
        assert!(!failed.is_transient());
    }
}
