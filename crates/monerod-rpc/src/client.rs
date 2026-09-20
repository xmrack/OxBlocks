//! Transport for the monerod daemon RPC.
//!
//! monerod exposes two different calling conventions and they fail differently:
//!
//! * `POST /json_rpc` — a JSON-RPC 2.0 envelope. Failures arrive as an `error`
//!   member alongside HTTP 200.
//! * `POST /<endpoint>` — a bare JSON body (`/get_transactions`, `/get_outs`,
//!   …). Failures arrive as a `"status"` string alongside HTTP 200.
//!
//! Both are normalised into [`RpcError`] here so that callers never have to
//! remember which convention a given call uses.

use std::time::Duration;

use serde::{Serialize, de::DeserializeOwned};

use crate::error::{RpcError, Status};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A connection to one monerod daemon.
///
/// Cheap to clone: the underlying connection pool is shared.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
}

#[derive(Debug, Clone)]
pub struct ClientBuilder {
    base: String,
    timeout: Duration,
    user_agent: Option<String>,
}

impl ClientBuilder {
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    pub fn build(self) -> Result<Client, RpcError> {
        let mut base = reqwest::Url::parse(&self.base)
            .map_err(|e| RpcError::BadUrl(self.base.clone(), e.to_string()))?;

        // `Url::join` replaces the last path segment unless the base ends in a
        // slash, so "http://host/mon" would resolve "/json_rpc" to "http://host/json_rpc"
        // and quietly talk to the wrong place. Normalise once, here.
        if !base.path().ends_with('/') {
            let fixed = format!("{}/", base.path());
            base.set_path(&fixed);
        }

        let mut http = reqwest::Client::builder()
            .timeout(self.timeout)
            // monerod never redirects. reqwest follows up to ten by default,
            // which would let anything sitting between us and the configured
            // URL -- a proxy, a misconfiguration, a compromised daemon --
            // point this client at a host the operator never named. There is
            // no legitimate redirect to follow, so follow none.
            .redirect(reqwest::redirect::Policy::none());
        if let Some(ua) = self.user_agent {
            http = http.user_agent(ua);
        }

        Ok(Client {
            http: http.build()?,
            base,
        })
    }
}

impl Client {
    pub fn builder(base_url: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            base: base_url.into(),
            timeout: DEFAULT_TIMEOUT,
            user_agent: None,
        }
    }

    pub fn new(base_url: impl Into<String>) -> Result<Self, RpcError> {
        Self::builder(base_url).build()
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// POST a body to `path` and return the parsed JSON, without interpreting it.
    async fn post(
        &self,
        path: &str,
        context: &'static str,
        body: &impl Serialize,
    ) -> Result<serde_json::Value, RpcError> {
        let url = self
            .base
            .join(path)
            .map_err(|e| RpcError::BadUrl(format!("{}{path}", self.base), e.to_string()))?;

        let response = self.http.post(url).json(body).send().await?;
        let http_status = response.status();
        let bytes = response.bytes().await?;

        if !http_status.is_success() {
            // Keep a bounded slice of the body: monerod's error pages are short,
            // but this is remote input and it ends up in logs.
            let body = String::from_utf8_lossy(&bytes);
            let body: String = body.chars().take(256).collect();
            return Err(RpcError::Http {
                context,
                status: http_status.as_u16(),
                body,
            });
        }

        serde_json::from_slice(&bytes).map_err(|source| RpcError::Decode { context, source })
    }

    /// Call a JSON-RPC 2.0 method on `/json_rpc`.
    pub async fn json_rpc<P, R>(
        &self,
        method: &'static str,
        params: Option<P>,
    ) -> Result<R, RpcError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        #[derive(Serialize)]
        struct Envelope<P> {
            jsonrpc: &'static str,
            id: &'static str,
            method: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            params: Option<P>,
        }

        let mut value = self
            .post(
                "json_rpc",
                method,
                &Envelope {
                    jsonrpc: "2.0",
                    id: "0",
                    method,
                    params,
                },
            )
            .await?;

        if let Some(error) = value.get("error") {
            return Err(RpcError::JsonRpc {
                method,
                code: error
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<no message>")
                    .to_owned(),
            });
        }

        let result =
            value
                .get_mut("result")
                .map(serde_json::Value::take)
                .ok_or(RpcError::Missing {
                    context: method,
                    field: "result",
                })?;

        // Several JSON-RPC results carry a `status` of their own in addition to
        // the envelope. An `error`-free response with `"status": "Failed"` is
        // still a failure.
        Self::check_status(&result, method)?;

        serde_json::from_value(result).map_err(|source| RpcError::Decode {
            context: method,
            source,
        })
    }

    /// Call one of the bare (non-JSON-RPC) endpoints, e.g. `/get_transactions`.
    ///
    /// `endpoint` is given without a leading slash.
    pub async fn endpoint<B, R>(&self, endpoint: &'static str, body: &B) -> Result<R, RpcError>
    where
        B: Serialize,
        R: DeserializeOwned,
    {
        let value = self.post(endpoint, endpoint, body).await?;
        Self::check_status(&value, endpoint)?;
        serde_json::from_value(value).map_err(|source| RpcError::Decode {
            context: endpoint,
            source,
        })
    }

    /// Reject a payload whose `status` is present and not `OK`.
    ///
    /// A missing `status` is tolerated: not every result carries one, and
    /// absence is not failure.
    fn check_status(value: &serde_json::Value, context: &'static str) -> Result<(), RpcError> {
        let Some(raw) = value.get("status").and_then(serde_json::Value::as_str) else {
            return Ok(());
        };
        let status = Status::parse(raw);
        if status.is_ok() {
            Ok(())
        } else {
            Err(RpcError::Status {
                endpoint: context,
                status,
            })
        }
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

    #[test]
    fn base_url_without_trailing_slash_still_resolves_under_its_path() {
        // The bug this guards: Url::join("json_rpc") against ".../mon" drops
        // "mon" and silently targets a different endpoint.
        let c = Client::new("http://127.0.0.1:18081/mon").expect("valid url");
        assert!(c.base_url().ends_with("/mon/"));
    }

    #[test]
    fn plain_host_is_accepted() {
        let c = Client::new("http://127.0.0.1:18081").expect("valid url");
        assert_eq!(c.base_url(), "http://127.0.0.1:18081/");
    }

    /// A redirect is an instruction to talk to somewhere the operator did not
    /// configure. monerod never sends one, so following one can only take this
    /// client somewhere it should not go.
    ///
    /// Answered by a real socket rather than by inspecting the builder: what
    /// matters is where the *request* ends up, and a redirect that was followed
    /// would show up here as a transport error against the unreachable port in
    /// the `Location` header instead of as the 302 itself.
    #[tokio::test]
    async fn a_redirect_is_reported_rather_than_followed() {
        use std::io::{Read, Write};

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port is available");
        let port = listener.local_addr().expect("bound").port();

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("the client connects");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout is settable");
            // Drain what arrives so the client is not writing into a closed
            // pipe; the contents do not matter.
            let mut scratch = [0u8; 4096];
            let _ = socket.read(&mut scratch);
            // Port 1 is not listening, so following this would fail loudly.
            socket
                .write_all(
                    b"HTTP/1.1 302 Found\r\n\
                      Location: http://127.0.0.1:1/json_rpc\r\n\
                      Content-Length: 0\r\n\r\n",
                )
                .expect("the response is written");
            let _ = socket.flush();
        });

        let client = Client::new(format!("http://127.0.0.1:{port}")).expect("valid url");
        let outcome: Result<serde_json::Value, _> = client.json_rpc("get_info", None::<()>).await;
        server.join().expect("the server thread finished");

        match outcome {
            Err(RpcError::Http { status, .. }) => assert_eq!(
                status, 302,
                "the redirect itself should come back as the answer"
            ),
            other => panic!("expected the 302 to be reported, got {other:?}"),
        }
    }

    #[test]
    fn nonsense_url_is_rejected_at_construction() {
        assert!(matches!(
            Client::new("not a url"),
            Err(RpcError::BadUrl(_, _))
        ));
    }

    #[test]
    fn non_ok_status_is_an_error_and_missing_status_is_not() {
        let failed = serde_json::json!({ "status": "Failed" });
        assert!(Client::check_status(&failed, "/get_outs").is_err());

        let busy = serde_json::json!({ "status": "BUSY" });
        let err = Client::check_status(&busy, "/get_outs").expect_err("BUSY is not OK");
        assert!(err.is_transient());

        let ok = serde_json::json!({ "status": "OK" });
        assert!(Client::check_status(&ok, "/get_outs").is_ok());

        let absent = serde_json::json!({ "height": 1 });
        assert!(Client::check_status(&absent, "/get_outs").is_ok());
    }
}
