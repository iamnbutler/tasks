//! Refuse browser-driven requests to the local API (#985).
//!
//! The API's access control was its bind address, and against a browser a
//! bind address is not access control. [`crate::server::actor_of`] reads a
//! request with no `X-Tasks-Actor` as [`crate::models::Actor::Human`], who is
//! never charter-gated, so any page you have open could drive the pipeline
//! two ways:
//!
//! 1. A CORS-*simple* `POST` — no body, no `Content-Type`, therefore no
//!    preflight. The opaque response does not matter, because
//!    `POST /tasks/{id}/build-now` has already dispatched a VM that writes
//!    code and opens pull requests.
//! 2. **DNS rebinding** — a name the attacker controls resolving to
//!    `127.0.0.1` makes their page genuinely same-origin, which lifts the
//!    simple-request restriction entirely: `/tasks`, `/decisions`, the
//!    transcripts, `POST /pull-requests/{n}/merge`.
//!
//! Two rules, applied by one middleware over the whole router. They are
//! **not interchangeable, because each is blind to the other's path**: the
//! rebind in (2) arrives with a perfectly ordinary `Origin` naming the
//! attacker's own site *and* a `Host` naming it too, while the simple POST in
//! (1) arrives with a loopback `Host` the rebind rule has no quarrel with.
//!
//! - The **authority** — every one the request states — must name this
//!   machine's own loopback ([`is_own_authority`]).
//! - An **`Origin` header**, any value at all, is a refusal. This API has no
//!   browser clients, so the header's *presence* is the finding.
//!
//! Both apply to reads as well as writes; deciding it per method means
//! re-deciding it for every route added later, which is the shape
//! [`crate::server::authorize`] exists not to have.
//!
//! # What this does not cover
//!
//! **A cross-site subresource `GET` straight to loopback.** Browsers do not
//! send `Origin` on `<img src>`, `<script src>`, `<link>` or `<iframe>`, so
//! `<img src="http://127.0.0.1:4800/…">` on any page you visit arrives with
//! no `Origin` and a `Host` of `127.0.0.1:4800`, passes both rules, and
//! reaches the handler. No rebinding is involved — the attacker addresses
//! loopback directly, which is the case the authority rule is not for.
//!
//! The residual is bounded to routes whose responses the attacker cannot read
//! (this API sends no CORS headers, so a cross-origin reader gets nothing)
//! and whose only effect is server-side. Today there is exactly one route
//! where that residual is not nil: **`GET /decisions/{seq}/reconcile`**,
//! which spends the server's *own* GitHub credential on an outbound call. It
//! is **accepted** rather than moved to `POST`: it is idempotent, it mutates
//! nothing locally, it answers only for a decision still `pending`, and the
//! obligation loop and the orchestrator's documented `curl` both name it as a
//! `GET`. What it costs an attacker who can make you load a page is one
//! GitHub read per pending decision — a rate-limit lever, not the
//! `build-now` hole. Closing it properly wants a `Sec-Fetch-Site` check,
//! which is a larger decision than this module.
//!
//! A browser old enough not to send `Origin` on a cross-site form `POST`
//! would also walk past the first rule. Every current browser sends it; this
//! is named so nobody reads the guard as a proof rather than a very good bar.
//!
//! # What this assumes
//!
//! **The loopback bind.** [`crate::server::bind`] takes
//! `Ipv4Addr::LOCALHOST` and has no knob, so the authority rule can be
//! absolute. If Tasks ever binds beyond loopback, this must be revisited
//! deliberately — the allow-list widened *and* something real put in front of
//! the port — rather than quietly relaxed. The broker
//! ([`crate::broker`], port 4801) is the one deliberate exclusion: it is a
//! second listener on purpose, reachable from the VM subnet, and every route
//! on it already demands a live lease. It builds its own router and must not
//! get this layer.

use std::net::{Ipv4Addr, Ipv6Addr};

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::http::header::{HOST, ORIGIN};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::warn;

use crate::server::ApiError;

/// How much of an offending value is repeated back in the sentence and the
/// log line. Both are attacker-controlled, and neither is worth an unbounded
/// copy of one; enough to recognise a rebind by name is enough.
const ECHO_LIMIT: usize = 96;

/// Why a request was refused.
///
/// Two variants and three sentences: a request that states *no* authority at
/// all deserves better than one about an empty name, and hyper only produces
/// it for a malformed HTTP/1.0-shaped request or an h2 stream with no
/// `:authority`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// An authority that is not this machine's loopback — or, as `None`, no
    /// authority stated at all.
    Host(Option<String>),
    /// An `Origin` header was present. The value is carried for the log; it
    /// is not consulted, because presence is the finding.
    Origin(String),
}

impl Refusal {
    /// The sentence sent back as the 403 body and written to the log.
    pub fn message(&self) -> String {
        match self {
            Refusal::Host(None) => "this request names no host; the Tasks API answers only \
                 requests addressed to its own loopback address"
                .to_string(),
            Refusal::Host(Some(host)) => format!(
                "host `{}` is not this machine's loopback; the Tasks API answers only requests \
                 addressed to 127.0.0.0/8, ::1 or localhost",
                echo(host)
            ),
            Refusal::Origin(origin) => format!(
                "this request carries an Origin header (`{}`); the Tasks API has no browser \
                 clients, so a request from a web page is refused whatever origin it names",
                echo(origin)
            ),
        }
    }
}

/// Truncate an attacker-controlled value to something a sentence can carry.
fn echo(value: &str) -> String {
    if value.chars().count() <= ECHO_LIMIT {
        return value.to_string();
    }
    let head: String = value.chars().take(ECHO_LIMIT).collect();
    format!("{head}…")
}

/// Does this authority name this machine's own loopback?
///
/// Accepts `127.0.0.0/8`, `::1` (bare or bracketed) and `localhost`, each
/// with or without a port. Addresses go through
/// [`Ipv4Addr::is_loopback`]/[`Ipv6Addr::is_loopback`] rather than string
/// matching, so `127.0.0.1`, `127.1.2.3` and `0:0:0:0:0:0:0:1` all answer
/// the same as their canonical spellings.
///
/// **The port is not compared**, only parsed. It is not what an attacker
/// differs on — a browser fills `Host` from the URL's *hostname*, so a rebind
/// arrives as `evil.example:4800` and fails on the host part alone — and
/// pinning it would refuse every test in this tree, which binds
/// `127.0.0.1:0`. Parsing it is still load-bearing: it is what stops
/// `127.0.0.1:80.evil.example` from walking past a naive split.
pub fn is_own_authority(authority: &str) -> bool {
    let Some(host) = host_of(authority) else {
        return false;
    };
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return v4.is_loopback();
    }
    if let Ok(v6) = host.parse::<Ipv6Addr>() {
        return v6.is_loopback();
    }
    host.eq_ignore_ascii_case("localhost")
}

/// The host part of an authority, or `None` if it is not one we will read.
///
/// IPv6 is where a naive split breaks. `[::1]:4800` must be unbracketed
/// before any colon split; bare `::1` has no port to split off at all and is
/// parsed whole. `::1:4800` is ambiguous by construction — it is also a
/// perfectly valid address that is simply not `::1` — and is refused either
/// way, since it reaches [`is_own_authority`]'s parse as a non-loopback
/// address.
///
/// Userinfo is refused outright: nothing here sends it, and `evil@127.0.0.1`
/// is exactly the shape a reader that only looked after the `@` would admit.
fn host_of(authority: &str) -> Option<&str> {
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if !after.is_empty() {
            after.strip_prefix(':')?.parse::<u16>().ok()?;
        }
        return (!host.is_empty()).then_some(host);
    }
    // More than one colon is a bare IPv6 literal: it has no port to split off,
    // so it is parsed whole and stands or falls as an address.
    if authority.matches(':').count() > 1 {
        return Some(authority);
    }
    match authority.split_once(':') {
        Some((host, port)) => {
            port.parse::<u16>().ok()?;
            (!host.is_empty()).then_some(host)
        }
        None => Some(authority),
    }
}

/// The whole decision, over a [`HeaderMap`] plus whatever authority the
/// request *line* states.
///
/// No axum types beyond `HeaderMap`, so it is unit-testable without a socket.
///
/// Two inputs because **HTTP/2 carries no `Host` header**: hyper puts
/// `:authority` in the URI and synthesizes nothing, so reading `Host` alone
/// would refuse a legitimate h2c client. Every authority the request states
/// is checked — an absolute-form request line must not be able to name a host
/// the header would have refused — and a request stating none is refused.
///
/// [`HeaderMap::get_all`] rather than `get`: two `Host` headers are malformed
/// and hyper ought to refuse them upstream, but "the first one is loopback"
/// is exactly the reading a smuggled second one is built to get. Checking all
/// of them leaves nothing to be first.
///
/// The authority is decided before the origin, so a request that states no
/// host at all is told the more fundamental thing about itself. Both are a
/// 403 and neither is a no-op, so the order costs nothing else.
pub fn verify(headers: &HeaderMap, uri_authority: Option<&str>) -> Result<(), Refusal> {
    let mut stated = 0usize;
    for value in headers.get_all(HOST) {
        stated += 1;
        let Ok(host) = value.to_str() else {
            return Err(Refusal::Host(Some(
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )));
        };
        if !is_own_authority(host) {
            return Err(Refusal::Host(Some(host.to_string())));
        }
    }
    if let Some(authority) = uri_authority {
        stated += 1;
        if !is_own_authority(authority) {
            return Err(Refusal::Host(Some(authority.to_string())));
        }
    }
    if stated == 0 {
        return Err(Refusal::Host(None));
    }
    if let Some(origin) = headers.get_all(ORIGIN).iter().next() {
        return Err(Refusal::Origin(
            String::from_utf8_lossy(origin.as_bytes()).into_owned(),
        ));
    }
    Ok(())
}

/// The [`axum::middleware::from_fn`] wrapper.
///
/// On a refusal it warns and answers [`ApiError::Forbidden`], so the 403 body
/// is the same `{"error": …}` shape every other refusal in this API uses.
pub async fn guard(request: Request, next: Next) -> Response {
    let authority = request.uri().authority().map(|a| a.as_str().to_owned());
    match verify(request.headers(), authority.as_deref()) {
        Ok(()) => next.run(request).await,
        Err(refusal) => {
            let message = refusal.message();
            warn!(
                method = %request.method(),
                path = %request.uri().path(),
                reason = %message,
                "refused a browser-shaped request to the local API"
            );
            ApiError::Forbidden(message).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    /// The shapes real clients actually send: `ureq` derives `Host` from the
    /// URL, so this is the app, the CLI and `tasks-client`; `reqwest` (`tasks
    /// reload`) and `curl -K` (the orchestrator) are the same three shapes.
    #[test]
    fn the_authorities_real_clients_send_are_accepted() {
        for authority in [
            "127.0.0.1",
            "127.0.0.1:4800",
            "127.0.0.1:0",
            "127.0.0.1:65535",
            "localhost",
            "localhost:4800",
            "LocalHost:4800",
            "::1",
            "[::1]",
            "[::1]:4800",
            "0:0:0:0:0:0:0:1",
            // 127.0.0.0/8 is loopback in its entirety, not just .0.1.
            "127.1.2.3:4800",
        ] {
            assert!(is_own_authority(authority), "should accept {authority}");
        }
    }

    /// A rebind arrives as the attacker's own name, because a browser fills
    /// `Host` from the URL's hostname and not from the address it resolved to.
    #[test]
    fn a_name_that_is_not_loopback_is_refused_however_it_resolves() {
        for authority in [
            "evil.example",
            "evil.example:4800",
            "localhost.evil.example:4800",
            "127.0.0.1.evil.example:4800",
            "notlocalhost",
            "10.0.0.1:4800",
            "0.0.0.0:4800",
            "192.168.64.1:4801",
            "[2001:db8::1]:4800",
        ] {
            assert!(!is_own_authority(authority), "should refuse {authority}");
        }
    }

    /// The port is parsed even though it is not compared, which is what stops
    /// a suffix from riding along behind a colon.
    #[test]
    fn a_port_that_is_not_a_port_is_refused() {
        assert!(!is_own_authority("127.0.0.1:80.evil.example"));
        assert!(!is_own_authority("127.0.0.1:"));
        assert!(!is_own_authority("127.0.0.1:65536"));
        assert!(!is_own_authority("localhost:evil"));
        assert!(!is_own_authority("[::1]:evil"));
        assert!(!is_own_authority("[::1]4800"));
    }

    /// IPv6 is where a naive split breaks: the brackets have to come off
    /// before any colon split, and a bare literal has no port to split off.
    #[test]
    fn ipv6_is_unbracketed_before_it_is_split() {
        assert!(is_own_authority("[::1]:4800"));
        assert!(is_own_authority("::1"));
        // Valid, and simply not ::1 — refused as an address rather than being
        // mistaken for `::1` with a port.
        assert!(!is_own_authority("::1:4800"));
        assert!(!is_own_authority("[]:4800"));
        assert!(!is_own_authority("[::1"));
    }

    /// `evil@127.0.0.1` is the shape a reader that only looked after the `@`
    /// would admit. Nothing here sends userinfo, so it is refused whole.
    #[test]
    fn userinfo_is_refused_rather_than_skipped_past() {
        assert!(!is_own_authority("evil.example@127.0.0.1"));
        assert!(!is_own_authority("127.0.0.1@evil.example"));
        assert!(!is_own_authority("@127.0.0.1"));
        assert!(!is_own_authority(""));
    }

    #[test]
    fn a_loopback_host_with_no_origin_is_the_ordinary_case() {
        assert_eq!(
            verify(&headers(&[("host", "127.0.0.1:4800")]), None),
            Ok(())
        );
    }

    /// HTTP/2 states its authority in the URI and sends no `Host` at all.
    #[test]
    fn an_authority_with_no_host_header_is_read_from_the_uri() {
        assert_eq!(verify(&HeaderMap::new(), Some("127.0.0.1:4800")), Ok(()));
        assert_eq!(
            verify(&HeaderMap::new(), Some("evil.example:4800")),
            Err(Refusal::Host(Some("evil.example:4800".into())))
        );
    }

    /// A request that states no authority anywhere is refused, and gets the
    /// sentence written for it rather than one about an empty name.
    #[test]
    fn a_request_that_states_no_authority_is_refused() {
        let refusal = verify(&HeaderMap::new(), None).unwrap_err();
        assert_eq!(refusal, Refusal::Host(None));
        assert!(refusal.message().contains("names no host"), "{refusal:?}");
    }

    /// "The first one is loopback" is exactly the reading a smuggled second
    /// `Host` is built to get, so every one of them is checked.
    #[test]
    fn a_second_host_header_cannot_hide_behind_the_first() {
        let map = headers(&[("host", "127.0.0.1:4800"), ("host", "evil.example")]);
        assert_eq!(
            verify(&map, None),
            Err(Refusal::Host(Some("evil.example".into())))
        );
    }

    /// An absolute-form request line must not be able to name a host the
    /// header would have refused.
    #[test]
    fn every_authority_the_request_states_is_checked() {
        let map = headers(&[("host", "127.0.0.1:4800")]);
        assert_eq!(
            verify(&map, Some("evil.example:4800")),
            Err(Refusal::Host(Some("evil.example:4800".into())))
        );
    }

    /// Any `Origin` at all, including `null`, including a loopback one. This
    /// API has no browser clients, so presence is the finding — and allowing
    /// a loopback origin would hand the API to anything else on this machine
    /// that serves HTML.
    #[test]
    fn any_origin_is_a_refusal_including_a_loopback_one() {
        for origin in [
            "https://evil.example",
            "null",
            "http://127.0.0.1:4800",
            "http://localhost:3000",
        ] {
            let map = headers(&[("host", "127.0.0.1:4800"), ("origin", origin)]);
            assert_eq!(
                verify(&map, None),
                Err(Refusal::Origin(origin.to_string())),
                "should refuse origin {origin}"
            );
        }
    }

    /// The sentences are what a human debugging a 403 reads, and an
    /// attacker-controlled value in one is bounded.
    #[test]
    fn the_sentences_name_the_rule_and_bound_the_value() {
        assert!(
            Refusal::Host(Some("evil.example".into()))
                .message()
                .contains("loopback")
        );
        assert!(
            Refusal::Origin("https://evil.example".into())
                .message()
                .contains("Origin")
        );
        let long = "x".repeat(500);
        let message = Refusal::Origin(long).message();
        assert!(message.len() < 400, "{} chars", message.len());
        assert!(message.contains('…'));
    }
}
