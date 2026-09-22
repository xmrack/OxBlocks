//! The daemon's base URL.
//!
//! Deliberately not a general URL implementation. This client talks to one
//! operator-configured daemon and appends a fixed set of endpoint names to it,
//! so the whole problem is: validate a base, and append a path segment.
//!
//! The `url` crate solves the general problem, and the general problem includes
//! internationalised domain names — which pulls `idna` and roughly twenty-five
//! ICU crates, megabytes of Unicode tables, to normalise hostnames that a
//! monerod URL never has.

use hyper::Uri;

/// A validated base URL, normalised to end in a slash.
#[derive(Debug, Clone)]
pub struct BaseUrl(String);

impl BaseUrl {
    /// Rejects anything that is not an absolute `http`/`https` URL.
    ///
    /// A query or fragment is refused rather than dropped: it means whoever
    /// configured this had something else in mind, and silently ignoring it
    /// would talk to a URL they did not ask for.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let uri: Uri = raw.parse().map_err(|e| format!("not a URL: {e}"))?;

        match uri.scheme_str() {
            Some("http" | "https") => {}
            Some(other) => return Err(format!("scheme {other} is not http or https")),
            None => return Err("no scheme; an absolute URL is required".to_owned()),
        }

        let authority = uri
            .authority()
            .ok_or_else(|| "no host".to_owned())?
            .as_str();
        if authority.is_empty() {
            return Err("no host".to_owned());
        }

        if let Some(pq) = uri.path_and_query()
            && pq.query().is_some()
        {
            return Err("a query string is not meaningful here".to_owned());
        }

        // `join` appends, so the base has to end where a segment can follow.
        // Without this, a base of ".../mon" would produce ".../monjson_rpc".
        let path = uri.path();
        let slash = if path.ends_with('/') { "" } else { "/" };
        let scheme = uri.scheme_str().unwrap_or("http");
        Ok(Self(format!("{scheme}://{authority}{path}{slash}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Appends one endpoint name.
    ///
    /// `path` must be a bare segment: every caller passes a literal monerod
    /// endpoint name (`json_rpc`, `get_outs`). Rejecting anything else keeps a
    /// future caller from smuggling `../`, a query, or a whole other URL into
    /// the address this client connects to.
    pub fn join(&self, path: &str) -> Result<Uri, String> {
        if path.is_empty() {
            return Err("empty endpoint".to_owned());
        }
        if !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(format!("{path} is not a bare endpoint name"));
        }
        format!("{}{path}", self.0)
            .parse()
            .map_err(|e| format!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_plain_host_gains_a_trailing_slash() {
        let b = BaseUrl::parse("http://127.0.0.1:18081").unwrap();
        assert_eq!(b.as_str(), "http://127.0.0.1:18081/");
    }

    /// The bug this guards: appending to ".../mon" would target ".../monjson_rpc",
    /// and with `url::Url::join` it silently targeted "/json_rpc" instead —
    /// either way, a different daemon endpoint than the operator configured.
    #[test]
    fn a_base_path_is_preserved_and_terminated() {
        let b = BaseUrl::parse("http://127.0.0.1:18081/mon").unwrap();
        assert_eq!(b.as_str(), "http://127.0.0.1:18081/mon/");
        assert_eq!(
            b.join("json_rpc").unwrap().to_string(),
            "http://127.0.0.1:18081/mon/json_rpc"
        );
    }

    #[test]
    fn an_already_terminated_base_is_not_doubled() {
        let b = BaseUrl::parse("http://127.0.0.1:18081/mon/").unwrap();
        assert_eq!(b.as_str(), "http://127.0.0.1:18081/mon/");
    }

    #[test]
    fn https_is_accepted_and_the_port_is_kept() {
        let b = BaseUrl::parse("https://node.example:18089").unwrap();
        assert_eq!(b.as_str(), "https://node.example:18089/");
        assert_eq!(
            b.join("get_outs").unwrap().to_string(),
            "https://node.example:18089/get_outs"
        );
    }

    #[test]
    fn joining_builds_the_endpoint_under_the_base() {
        let b = BaseUrl::parse("http://127.0.0.1:18081").unwrap();
        let uri = b.join("json_rpc").unwrap();
        assert_eq!(uri.to_string(), "http://127.0.0.1:18081/json_rpc");
        assert_eq!(uri.path(), "/json_rpc");
        assert_eq!(uri.host(), Some("127.0.0.1"));
        assert_eq!(uri.port_u16(), Some(18081));
        assert_eq!(uri.scheme_str(), Some("http"));
    }

    #[test]
    fn a_url_without_a_scheme_or_host_is_refused() {
        assert!(BaseUrl::parse("not a url").is_err());
        assert!(BaseUrl::parse("127.0.0.1:18081").is_err());
        assert!(BaseUrl::parse("/json_rpc").is_err());
        assert!(BaseUrl::parse("").is_err());
    }

    /// A daemon is reached over HTTP. Anything else is a misconfiguration that
    /// would otherwise surface as a confusing connection failure much later.
    #[test]
    fn a_non_http_scheme_is_refused() {
        assert!(BaseUrl::parse("file:///etc/passwd").is_err());
        assert!(BaseUrl::parse("ftp://host/").is_err());
        assert!(BaseUrl::parse("ws://host/").is_err());
    }

    #[test]
    fn a_query_string_is_refused_rather_than_dropped() {
        assert!(BaseUrl::parse("http://127.0.0.1:18081/?a=1").is_err());
    }

    /// Every caller passes a literal, but the check is what keeps that true.
    #[test]
    fn only_a_bare_endpoint_name_can_be_joined() {
        let b = BaseUrl::parse("http://127.0.0.1:18081/mon/").unwrap();
        assert!(b.join("get_outs").is_ok());
        assert!(b.join("get-outs").is_ok());
        assert!(b.join("").is_err());
        assert!(b.join("../json_rpc").is_err());
        assert!(b.join("/json_rpc").is_err());
        assert!(b.join("json_rpc?x=1").is_err());
        assert!(b.join("http://elsewhere/json_rpc").is_err());
        assert!(b.join("json rpc").is_err());
    }
}
