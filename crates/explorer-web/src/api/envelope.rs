//! The response envelope, byte-compatible with the C++ explorer's JSON API.
//!
//! Upstream wraps every answer in a JSend-ish object and **always** replies
//! HTTP 200, including for failures. Clients distinguish outcomes by the
//! `status` member, not by the status code.
//!
//! ```text
//! {"data": <object>, "status": "success"}
//! {"data": {"title": "<message>"}, "status": "fail"}
//! {"data": null, "message": "<message>", "status": "error"}
//! ```
//!
//! Note `"data": null` on the error form. The researched spec asserted `{}`
//! and reasoned about it in prose; a verifier compiled the vendored
//! nlohmann 3.12.0 and showed an empty braced-init-list picks the default
//! constructor, giving `value_t::null`. Upstream's own code proves it too:
//! `page.h:4550` initialises `mixins` the same way and then calls
//! `push_back`, which only converts `null` to an array and throws on an
//! object. Eleven documented failure forms were byte-wrong on that point.

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// `fail` is the caller's fault — a hash that will not parse, a height past
/// the tip. `error` is ours or the daemon's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Fail,
    Error,
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub outcome: Outcome,
    pub message: String,
    /// Upstream sometimes returns partially-built data alongside an error —
    /// `/api/transactions` assigns `data["blocks"]` before the loop that can
    /// fail, so its error form carries whatever blocks were collected. When
    /// that applies, the partial value goes here.
    pub partial: Option<serde_json::Value>,
}

impl ApiError {
    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Fail,
            message: message.into(),
            partial: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Error,
            message: message.into(),
            partial: None,
        }
    }

    #[must_use]
    pub fn with_partial(mut self, partial: serde_json::Value) -> Self {
        self.partial = Some(partial);
        self
    }

    fn to_value(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        match self.outcome {
            Outcome::Fail => {
                let mut data = serde_json::Map::new();
                data.insert(
                    "title".to_owned(),
                    serde_json::Value::String(self.message.clone()),
                );
                out.insert("data".to_owned(), serde_json::Value::Object(data));
                out.insert(
                    "status".to_owned(),
                    serde_json::Value::String("fail".to_owned()),
                );
            }
            Outcome::Error => {
                out.insert(
                    "data".to_owned(),
                    self.partial.clone().unwrap_or(serde_json::Value::Null),
                );
                out.insert(
                    "message".to_owned(),
                    serde_json::Value::String(self.message.clone()),
                );
                out.insert(
                    "status".to_owned(),
                    serde_json::Value::String("error".to_owned()),
                );
            }
        }
        serde_json::Value::Object(out)
    }
}

#[derive(Debug, Clone)]
pub struct ApiOk<T>(pub T);

/// Headers upstream sets on every JSON response (`main.cpp:29-37`).
fn api_headers() -> [(HeaderName, HeaderValue); 3] {
    [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type"),
        ),
    ]
}

fn render(value: &serde_json::Value) -> Response {
    // `dump()` with no arguments is compact, and serde_json's default writer
    // matches: no spaces after `,` or `:`.
    let body = serde_json::to_string(value).unwrap_or_else(|_| {
        r#"{"data":null,"message":"serialisation failed","status":"error"}"#.to_owned()
    });
    (StatusCode::OK, api_headers(), body).into_response()
}

impl<T: Serialize> IntoResponse for ApiOk<T> {
    fn into_response(self) -> Response {
        let data = match serde_json::to_value(&self.0) {
            Ok(v) => v,
            Err(e) => return ApiError::error(format!("could not render: {e}")).into_response(),
        };
        let mut out = serde_json::Map::new();
        out.insert("data".to_owned(), data);
        out.insert(
            "status".to_owned(),
            serde_json::Value::String("success".to_owned()),
        );
        render(&serde_json::Value::Object(out))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        render(&self.to_value())
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

    fn body_of(r: Response) -> String {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async move {
                let bytes = axum::body::to_bytes(r.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                String::from_utf8(bytes.to_vec()).unwrap()
            })
    }

    #[test]
    fn a_failure_is_data_title_and_http_200() {
        // The wording is upstream's, from a live deployment: it echoes the
        // sanitised argument, and spells "Cant" without an apostrophe.
        let r = ApiError::fail("Cant parse tx hash: abc").into_response();
        assert_eq!(r.status(), StatusCode::OK, "upstream never uses 4xx here");
        assert_eq!(
            body_of(r),
            r#"{"data":{"title":"Cant parse tx hash: abc"},"status":"fail"}"#
        );
    }

    /// The correction: `data` is null, not `{}`. A verifier compiled nlohmann
    /// to establish this after the spec asserted the opposite.
    #[test]
    fn an_error_carries_a_null_data_and_a_message() {
        let r = ApiError::error("boom").into_response();
        assert_eq!(
            body_of(r),
            r#"{"data":null,"message":"boom","status":"error"}"#
        );
    }

    /// `/api/transactions` returns whatever blocks it had collected before the
    /// failure, because upstream assigns the array before the loop.
    #[test]
    fn an_error_can_carry_partially_built_data() {
        let r = ApiError::error("Cant get block: 99")
            .with_partial(serde_json::json!({"blocks": [{"height": 100}]}))
            .into_response();
        assert_eq!(
            body_of(r),
            r#"{"data":{"blocks":[{"height":100}]},"message":"Cant get block: 99","status":"error"}"#
        );
    }

    #[test]
    fn success_wraps_the_payload_and_sets_the_cors_headers() {
        #[derive(Serialize)]
        struct Payload {
            // Declared alphabetically on purpose; see `keys_are_alphabetical`.
            api: u64,
            height: u64,
        }
        let r = ApiOk(Payload {
            api: 65539,
            height: 7,
        })
        .into_response();

        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(
            r.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        assert_eq!(
            body_of(r),
            r#"{"data":{"api":65539,"height":7},"status":"success"}"#
        );
    }

    /// nlohmann stores objects in a `std::map`, so upstream emits keys in
    /// byte-ascending order, recursively. serde emits struct fields in
    /// *declaration* order, so every response struct must declare its fields
    /// alphabetically. This checks the envelope itself; the response shapes
    /// are covered by `shapes::tests::declaration_order_is_alphabetical`.
    #[test]
    fn envelope_keys_are_alphabetical() {
        let e = ApiError::error("x").into_response();
        let body = body_of(e);
        let keys: Vec<&str> = ["data", "message", "status"].into();
        let mut last = 0usize;
        for k in keys {
            let at = body.find(&format!("\"{k}\"")).expect("key present");
            assert!(at >= last, "{k} is out of alphabetical order");
            last = at;
        }
    }
}
