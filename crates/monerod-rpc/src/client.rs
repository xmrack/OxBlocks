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

use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes};
use hyper::header::{ACCEPT, CONTENT_TYPE, HeaderValue, USER_AGENT};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use serde::{Serialize, de::DeserializeOwned};

use crate::error::{RpcError, Status, TransportKind};
use crate::url::BaseUrl;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a failing response body is kept for the error message.
///
/// This is remote input on its way into logs, so it is bounded. monerod's own
/// error bodies are far shorter than this.
const MAX_ERROR_BODY: usize = 256;

/// The largest response this client will accumulate, unless told otherwise.
///
/// hyper hands back a stream; without a ceiling, a daemon that answered with
/// an endless body would grow this process until it died. `/get_transactions`
/// over a wide block range is the largest legitimate response and is nowhere
/// near this.
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(feature = "tls")]
type Connector = hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;
#[cfg(not(feature = "tls"))]
type Connector = hyper_util::client::legacy::connect::HttpConnector;

/// A connection to one monerod daemon.
///
/// Cheap to clone: the underlying connection pool is shared.
#[derive(Debug, Clone)]
pub struct Client {
    http: HyperClient<Connector, Full<Bytes>>,
    base: BaseUrl,
    timeout: Duration,
    max_response_bytes: u64,
    user_agent: Option<HeaderValue>,
}

#[derive(Debug, Clone)]
pub struct ClientBuilder {
    base: String,
    timeout: Duration,
    max_response_bytes: u64,
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

    /// The ceiling on a single response body.
    pub fn max_response_bytes(mut self, bytes: u64) -> Self {
        self.max_response_bytes = bytes;
        self
    }

    pub fn build(self) -> Result<Client, RpcError> {
        let base =
            BaseUrl::parse(&self.base).map_err(|e| RpcError::BadUrl(self.base.clone(), e))?;

        let user_agent = self
            .user_agent
            .map(|ua| {
                HeaderValue::from_str(&ua)
                    .map_err(|e| RpcError::BadUrl(ua.clone(), format!("bad user agent: {e}")))
            })
            .transpose()?;

        // Nothing here follows redirects: hyper's client does not, and there
        // is no legitimate redirect for monerod to send. Anything sitting
        // between this client and the configured URL -- a proxy, a
        // misconfiguration, a compromised daemon -- therefore cannot point it
        // at a host the operator never named.
        Ok(Client {
            http: HyperClient::builder(TokioExecutor::new()).build(connector()),
            base,
            timeout: self.timeout,
            max_response_bytes: self.max_response_bytes,
            user_agent,
        })
    }
}

impl Client {
    pub fn builder(base_url: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            base: base_url.into(),
            timeout: DEFAULT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            user_agent: None,
        }
    }

    pub fn new(base_url: impl Into<String>) -> Result<Self, RpcError> {
        Self::builder(base_url).build()
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// Reads a response body, refusing one that never ends.
    ///
    /// Frame by frame with a running total, rather than collecting and then
    /// measuring: a chunked response declares no length, so measuring what
    /// arrived would mean the memory had already been taken before the limit
    /// was consulted. This stops at the first frame that crosses it.
    async fn collect_body(
        &self,
        response: hyper::Response<hyper::body::Incoming>,
        context: &'static str,
    ) -> Result<Bytes, RpcError> {
        let too_large = |len| RpcError::ResponseTooLarge { context, len };

        // A declared length over the ceiling is refused before reading at all.
        if let Some(len) = response.body().size_hint().exact()
            && len > self.max_response_bytes
        {
            return Err(too_large(len));
        }

        let mut body = response.into_body();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| RpcError::Transport {
                context,
                kind: TransportKind::Other,
                message: e.to_string(),
            })?;
            let Some(chunk) = frame.data_ref() else {
                continue;
            };
            let total = (buf.len() as u64).saturating_add(chunk.len() as u64);
            if total > self.max_response_bytes {
                return Err(too_large(total));
            }
            buf.extend_from_slice(chunk);
        }
        Ok(Bytes::from(buf))
    }

    /// POST a body to `path` and return the parsed JSON, without interpreting it.
    async fn post(
        &self,
        path: &str,
        context: &'static str,
        body: &impl Serialize,
    ) -> Result<serde_json::Value, RpcError> {
        let uri = self
            .base
            .join(path)
            .map_err(|e| RpcError::BadUrl(format!("{}{path}", self.base.as_str()), e))?;

        let payload =
            serde_json::to_vec(body).map_err(|source| RpcError::Encode { context, source })?;

        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .body(Full::new(Bytes::from(payload)))
            .map_err(|e| RpcError::BadUrl(path.to_owned(), e.to_string()))?;
        if let Some(ua) = &self.user_agent {
            request.headers_mut().insert(USER_AGENT, ua.clone());
        }

        // One deadline across the whole exchange -- connect, send, read the
        // head, read the body -- rather than one per stage, which would let a
        // daemon that stalls in each stage in turn take twice as long as the
        // operator configured. reqwest applied its timeout the same way.
        let deadline = tokio::time::Instant::now() + self.timeout;
        let expired = |stage: &str| RpcError::Transport {
            context,
            kind: TransportKind::Timeout,
            message: format!("{stage} within {:?}", self.timeout),
        };

        let sent = tokio::time::timeout_at(deadline, self.http.request(request))
            .await
            .map_err(|_| expired("no response"))?;

        let response = sent.map_err(|e| RpcError::Transport {
            context,
            kind: if e.is_connect() {
                TransportKind::Connect
            } else {
                TransportKind::Other
            },
            message: e.to_string(),
        })?;

        let http_status = response.status();
        let bytes = tokio::time::timeout_at(deadline, self.collect_body(response, context))
            .await
            .map_err(|_| expired("body not read"))??;

        if !http_status.is_success() {
            // Keep a bounded slice of the body: monerod's error pages are short,
            // but this is remote input and it ends up in logs.
            let body = String::from_utf8_lossy(&bytes);
            let body: String = body.chars().take(MAX_ERROR_BODY).collect();
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

/// The connector, with TLS when the feature is on.
///
/// `https_or_http` rather than `https_only`: the overwhelmingly common
/// deployment is a loopback daemon over plain HTTP, and requiring TLS there
/// would break it. The scheme in the configured URL decides.
#[cfg(feature = "tls")]
fn connector() -> Connector {
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.enforce_http(false);
    hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .wrap_connector(http)
}

#[cfg(not(feature = "tls"))]
fn connector() -> Connector {
    hyper_util::client::legacy::connect::HttpConnector::new()
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

    /// A one-shot loopback server that cannot outlive the test.
    ///
    /// `TcpListener::accept` has no timeout, so a blocking server thread waits
    /// for a client that a broken build may never send -- turning a failing
    /// assertion into a hung test, which in CI is a job timeout with no
    /// message. Everything here is bounded: accept, read and join.
    struct Peer {
        port: u16,
        handle: std::thread::JoinHandle<String>,
    }

    /// Serves `body` as a JSON 200, with the length computed rather than typed.
    fn serve_json(body: &str) -> Peer {
        serve_owned(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
    }

    /// Announces a body far over any sane ceiling, then sends almost none of
    /// it. Refusing on the declared length means never reading the rest.
    fn serve_declared_huge() -> Peer {
        serve_owned(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10000000\r\n\r\n{}"
                .to_owned(),
        )
    }

    /// A chunked 200 that declares no length and sends more than it should.
    fn serve_chunked() -> Peer {
        let chunk = "x".repeat(512);
        let mut reply =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
                .to_owned();
        for _ in 0..8 {
            reply.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
        }
        reply.push_str("0\r\n\r\n");
        serve_owned(reply)
    }

    /// Sends a complete head promising a body that never arrives.
    fn serve_truncated_body() -> Peer {
        serve_owned(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n{"
                .to_owned(),
        )
    }

    /// Accepts, then never answers -- the client's deadline is what ends it.
    fn serve_nothing() -> Peer {
        serve_owned(String::new())
    }

    fn serve(reply: &'static [u8]) -> Peer {
        serve_owned(String::from_utf8_lossy(reply).into_owned())
    }

    fn serve_owned(reply: String) -> Peer {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port is available");
        let port = listener.local_addr().expect("bound").port();
        listener
            .set_nonblocking(true)
            .expect("the listener can be polled");

        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                if std::time::Instant::now() > deadline {
                    return String::new();
                }
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return String::new(),
                }
            };
            socket
                .set_nonblocking(false)
                .expect("back to blocking for the exchange");
            let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
            let mut scratch = [0u8; 8192];
            let n = socket.read(&mut scratch).unwrap_or(0);
            let stall_after_writing = reply.contains("Content-Length: 4096");
            if reply.is_empty() {
                // Hold the connection open with no response, so the only thing
                // that can end the exchange is the client giving up.
                std::thread::sleep(Duration::from_secs(10));
            } else {
                let _ = socket.write_all(reply.as_bytes());
                let _ = socket.flush();
                if stall_after_writing {
                    std::thread::sleep(Duration::from_secs(10));
                }
            }
            String::from_utf8_lossy(scratch.get(..n).unwrap_or_default()).into_owned()
        });

        Peer { port, handle }
    }

    impl Peer {
        /// The request bytes the client actually sent, or empty if it never
        /// connected.
        fn request(self) -> String {
            self.handle.join().expect("the server thread finished")
        }
    }

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
        // Port 1 is not listening, so a followed redirect would fail loudly.
        let peer = serve(
            b"HTTP/1.1 302 Found\r\n\
              Location: http://127.0.0.1:1/json_rpc\r\n\
              Content-Length: 0\r\n\r\n",
        );

        let client = Client::new(format!("http://127.0.0.1:{}", peer.port)).expect("valid url");
        let outcome: Result<serde_json::Value, _> = client.json_rpc("get_info", None::<()>).await;
        assert!(!peer.request().is_empty(), "the client never connected");

        match outcome {
            Err(RpcError::Http { status, .. }) => assert_eq!(
                status, 302,
                "the redirect itself should come back as the answer"
            ),
            other => panic!("expected the 302 to be reported, got {other:?}"),
        }
    }

    /// The TLS stack must be usable at runtime, not merely linked.
    ///
    /// rustls 0.23 resolves its crypto provider at *run* time and panics with
    /// "no process-level CryptoProvider available" if none was installed. That
    /// is invisible to `cargo check` and to every test that only speaks plain
    /// HTTP, so it would first appear as a crash on an operator's first
    /// request to an https daemon.
    ///
    /// The peer here answers with garbage rather than a ServerHello, so the
    /// handshake fails -- which is the point: reaching a handshake failure
    /// means the TLS path ran.
    #[cfg(feature = "tls")]
    #[tokio::test]
    async fn the_tls_connector_reaches_a_handshake_rather_than_panicking() {
        // Not a TLS record, so the client rejects it as a bad peer.
        let peer = serve(b"definitely not a ServerHello\r\n");

        let client = Client::builder(format!("https://127.0.0.1:{}", peer.port))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("valid url");
        let outcome: Result<serde_json::Value, _> = client.json_rpc("get_info", None::<()>).await;
        assert!(
            !peer.request().is_empty(),
            "no TLS ClientHello arrived, so the handshake was never attempted"
        );

        match outcome {
            Err(RpcError::Transport { .. }) => {}
            other => panic!("expected a transport failure from the handshake, got {other:?}"),
        }
    }

    /// A plain-HTTP daemon must still work when the TLS feature is on: the
    /// connector is `https_or_http`, and the configured scheme decides.
    #[tokio::test]
    async fn a_plain_http_daemon_is_reachable_with_tls_compiled_in() {
        let peer = serve_json(r#"{"id":"0","jsonrpc":"2.0","result":{"status":"OK","height":7}}"#);

        let client = Client::new(format!("http://127.0.0.1:{}", peer.port)).expect("valid url");
        let got: serde_json::Value = client
            .json_rpc("get_info", None::<()>)
            .await
            .expect("the daemon answered");
        assert_eq!(got.get("height").and_then(|h| h.as_u64()), Some(7));
        assert!(!peer.request().is_empty(), "the client never connected");
    }

    /// The request monerod actually receives: a POST of JSON to the endpoint
    /// under the configured base path, with no redirect handling in between.
    #[tokio::test]
    async fn the_request_is_a_json_post_to_the_configured_path() {
        let peer = serve_json(r#"{"result":{}}"#);

        let client = Client::builder(format!("http://127.0.0.1:{}/mon", peer.port))
            .user_agent("oxblocks-test/1")
            .build()
            .expect("valid url");
        let _: Result<serde_json::Value, _> = client.json_rpc("get_info", None::<()>).await;
        let request = peer.request();

        assert!(
            request.starts_with("POST /mon/json_rpc HTTP/1.1"),
            "wrong method or path: {request}"
        );
        assert!(
            request.contains("content-type: application/json")
                || request.contains("Content-Type: application/json"),
            "no json content type: {request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("user-agent: oxblocks-test/1"),
            "the user agent was not sent: {request}"
        );
        assert!(
            request.contains(r#""method":"get_info""#),
            "the JSON-RPC body did not arrive: {request}"
        );
    }

    /// A daemon that accepts the connection and then says nothing must not
    /// pin the request open. The explorer's own request deadline is set below
    /// this one, so without it a stalled daemon would outlive the reader
    /// waiting on it.
    #[tokio::test]
    async fn a_daemon_that_never_answers_hits_the_deadline() {
        let peer = serve_nothing();
        let client = Client::builder(format!("http://127.0.0.1:{}", peer.port))
            .timeout(Duration::from_millis(250))
            .build()
            .expect("valid url");

        let started = std::time::Instant::now();
        let outcome: Result<serde_json::Value, _> = client.json_rpc("get_info", None::<()>).await;
        let waited = started.elapsed();

        match outcome {
            Err(e @ RpcError::Transport { .. }) => {
                assert!(e.is_transient(), "a timeout is worth retrying");
                assert!(
                    e.to_string().contains("get_info"),
                    "the failure should name the call: {e}"
                );
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(
            waited < Duration::from_secs(5),
            "waited {waited:?}, so the configured deadline was not what stopped it"
        );
    }

    /// The head arriving is not the exchange finishing. A daemon that sends a
    /// Content-Length and then withholds the body would hang a request that
    /// only bounded the response head.
    #[tokio::test]
    async fn a_body_that_never_arrives_hits_the_same_deadline() {
        let peer = serve_truncated_body();
        let client = Client::builder(format!("http://127.0.0.1:{}", peer.port))
            .timeout(Duration::from_millis(250))
            .build()
            .expect("valid url");

        let started = std::time::Instant::now();
        let outcome: Result<serde_json::Value, _> = client.json_rpc("get_info", None::<()>).await;
        let waited = started.elapsed();

        assert!(
            matches!(outcome, Err(RpcError::Transport { .. })),
            "expected the body read to time out, got {outcome:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "waited {waited:?}, so nothing bounded the body read"
        );
    }

    /// An oversized body is refused, whether or not it announced its size.
    ///
    /// The declared-length case is refused before a byte is read. The chunked
    /// case has no declared length, so it is refused at the first frame that
    /// crosses the ceiling -- which is the point: measuring after collecting
    /// would mean the memory had already been taken.
    #[tokio::test]
    async fn a_response_over_the_ceiling_is_refused() {
        for (label, peer) in [
            ("declared length", serve_json(&"x".repeat(4096))),
            ("chunked", serve_chunked()),
        ] {
            let client = Client::builder(format!("http://127.0.0.1:{}", peer.port))
                .timeout(Duration::from_secs(5))
                .max_response_bytes(1024)
                .build()
                .expect("valid url");
            let outcome: Result<serde_json::Value, _> =
                client.json_rpc("get_info", None::<()>).await;
            match outcome {
                Err(RpcError::ResponseTooLarge { len, .. }) => {
                    assert!(len > 1024, "{label}: refused at {len}, under the ceiling");
                }
                other => panic!("{label}: expected a size refusal, got {other:?}"),
            }
        }
    }

    /// The same body, under the ceiling, is read normally -- so the test above
    /// is measuring the limit rather than a transport that never works.
    /// A declared length over the ceiling is refused on the strength of the
    /// declaration, without reading the body it promises.
    ///
    /// Distinguishable from the streaming check because this peer announces
    /// ten megabytes and then sends two bytes: refusing on the header reports
    /// the declared size, while reading first would stall waiting for a body
    /// that never finishes arriving.
    #[tokio::test]
    async fn a_declared_length_over_the_ceiling_is_refused_before_reading() {
        let peer = serve_declared_huge();
        let client = Client::builder(format!("http://127.0.0.1:{}", peer.port))
            .timeout(Duration::from_millis(500))
            .max_response_bytes(1024)
            .build()
            .expect("valid url");

        let outcome: Result<serde_json::Value, _> = client.json_rpc("get_info", None::<()>).await;
        match outcome {
            Err(RpcError::ResponseTooLarge { len, .. }) => assert_eq!(
                len, 10_000_000,
                "the refusal should quote the declared length, not what arrived"
            ),
            other => panic!("expected refusal on the declared length, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_response_under_the_ceiling_is_read() {
        let peer = serve_json(r#"{"result":{"status":"OK","height":7}}"#);
        let client = Client::builder(format!("http://127.0.0.1:{}", peer.port))
            .max_response_bytes(1024)
            .build()
            .expect("valid url");
        let got: serde_json::Value = client
            .json_rpc("get_info", None::<()>)
            .await
            .expect("a small response is fine");
        assert_eq!(got.get("height").and_then(|h| h.as_u64()), Some(7));
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
