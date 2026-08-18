//! Credential scrubbing for anything an agent, or git, hands us as text.
//!
//! The server mints `https://x-access-token:<token>@github.com/o/r.git` clone
//! URLs and hands them to VMs. Git echoes its remote back — in `git remote
//! -v`, in most of its errors — so that URL turns up in agent stdout, in
//! stderr, and in the error strings we build ourselves. Every one of those
//! paths ends somewhere durable and readable: `transcript_lines`,
//! `sessions.exit_reason`, the event log, the log file.
//!
//! So the scrub lives at the *write* choke points rather than at each caller,
//! and is deliberately safe to apply twice — see the idempotency test.
//!
//! # Why it lives here rather than in `tasks`
//!
//! Both supervisors mint the same leak from *inside* a VM — a clone that
//! fails, a command line they cannot decode — and neither depends on the
//! `tasks` crate. This is the lowest place the server and both supervisors
//! share one implementation, so there is one set of rules rather than three
//! that drift. `crate::redact` in `tasks` is a re-export of this module.
//!
//! It is deliberately **not** shared with `vm_pool_protocol::redact`, which
//! solves the same problem one crate over: `crates/vm-pool/*` must never
//! depend on a tasks crate, and pointing `tasks` at vm-pool instead would put
//! a security control inside a vendored crate that is meant to stay
//! independently publishable and swappable. The two are not identical
//! anyway — only vm-pool's redacts environment *values*, because nothing on
//! this side formats an environment vector.
//!
//! # Not a substitute for rotation
//!
//! Scrubbing stops *new* writes. A credential that has already been written
//! to a log, a console or an archive of one stays exposed until a human
//! rotates it; nothing here can do that.

use std::borrow::Cow;
use std::fmt;

/// Strip credentials from URLs embedded in text — clone URLs carry
/// `x-access-token:<token>@`, and git repeats the URL in its errors, which
/// flow into `exit_reason`, the event log, and the API.
///
/// Credential-aware rather than naive: only the userinfo of a `scheme://`
/// URL's authority is stripped. An `@` later in a path is left alone.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        let (before, after) = rest.split_at(idx + 3);
        out.push_str(before);
        // Authority runs to the first `/`, `?`, `#`, whitespace, or quote.
        let authority_end = after
            .find(['/', '?', '#', ' ', '\t', '\n', '\r', '\'', '"'])
            .unwrap_or(after.len());
        let authority = &after[..authority_end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &after[authority_end..];
    }
    out.push_str(rest);
    out
}

/// Cheap negative check: no `://` or no `@` means [`redact`] provably cannot
/// change the text, because it only ever rewrites the userinfo of a
/// `scheme://` authority.
fn may_contain_credentials(text: &str) -> bool {
    text.contains("://") && text.contains('@')
}

/// [`redact`] for the hot path — every transcript line goes through here, and
/// the overwhelming majority are plain agent output with no URL in them at
/// all. Borrows in that case; only a line that could hold a credential is
/// scanned and copied.
pub fn redact_line(text: &str) -> Cow<'_, str> {
    if may_contain_credentials(text) {
        Cow::Owned(redact(text))
    } else {
        Cow::Borrowed(text)
    }
}

/// [`redact_line`] for a `String` the caller already owns: returns it
/// untouched, with no allocation, when there is nothing to scrub.
pub fn redact_owned(text: String) -> String {
    if may_contain_credentials(&text) {
        redact(&text)
    } else {
        text
    }
}

/// A credential held in memory with no printable rendering.
///
/// `Debug` and `Display` are both `<redacted>`, and the value is reachable
/// only through [`Secret::expose`] — so a type that holds one can keep its
/// `#[derive(Debug)]`, and the next `tracing` field over it is safe by
/// construction rather than by review. That is the whole point: the leaks
/// this module cleans up were all one derived formatter away from a log sink.
///
/// `expose` is named to be conspicuous at the call site. There are meant to be
/// very few of them.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The real value, for handing to whatever has to authenticate with it.
    /// Never for logging.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_credentials_but_not_path_ats() {
        assert_eq!(
            redact(
                "fatal: could not read from 'https://x-access-token:ghp_abc123@github.com/o/r.git'"
            ),
            "fatal: could not read from 'https://***@github.com/o/r.git'"
        );
        assert_eq!(
            redact("https://user@host/path and /a/path@with-at stays"),
            "https://***@host/path and /a/path@with-at stays"
        );
        // No credentials, no change.
        assert_eq!(
            redact("https://github.com/o/r.git plain"),
            "https://github.com/o/r.git plain"
        );
        assert_eq!(redact("no urls at all"), "no urls at all");
    }

    /// Load-bearing: the sink scrubs, and so does the store behind it, so
    /// nearly every line is redacted twice. A second pass must be a no-op —
    /// otherwise `***@host` would decay into `***@` at some later hop.
    #[test]
    fn redaction_is_idempotent() {
        for input in [
            "fatal: 'https://x-access-token:ghp_abc123@github.com/o/r.git' not found",
            "https://user@host/path and /a/path@with-at stays",
            "https://github.com/o/r.git plain",
            "ssh://git@github.com:22/o/r.git",
            "no urls at all",
        ] {
            let once = redact(input);
            assert_eq!(redact(&once), once, "second pass changed {once:?}");
        }
    }

    #[test]
    fn redact_line_borrows_when_there_is_nothing_to_scrub() {
        assert!(matches!(
            redact_line("plain agent output, no url"),
            Cow::Borrowed(_)
        ));
        // An `@` with no scheme is not a credential — mail-shaped log lines
        // are common and must not cost an allocation either.
        assert!(matches!(
            redact_line("committing as test@example.com"),
            Cow::Borrowed(_)
        ));
        assert_eq!(
            redact_line("cloning https://x-access-token:ghp_abc@github.com/o/r.git"),
            "cloning https://***@github.com/o/r.git"
        );
    }

    /// The value that cannot authenticate anywhere, per the rule that no test
    /// in this change may carry a real credential.
    const FAKE: &str = "not-a-real-credential-0000";

    /// A `Secret` has no printable rendering at all, which is what lets the
    /// types that hold one keep a derived `Debug`.
    #[test]
    fn a_secret_never_prints_itself() {
        let secret = Secret::new(FAKE);
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert_eq!(format!("{secret}"), "<redacted>");
        // A struct holding one is safe to `{:?}` — the hazard this closes.
        // Read only through the derived `Debug`, which is the point.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            token: Option<Secret>,
            branch: &'static str,
        }
        let rendered = format!(
            "{:?}",
            Holder {
                token: Some(secret.clone()),
                branch: "main",
            }
        );
        assert!(!rendered.contains(FAKE), "{rendered}");
        assert!(rendered.contains("main"), "{rendered}");
        // And the value is still there for whoever has to authenticate.
        assert_eq!(secret.expose(), FAKE);
    }

    #[test]
    fn redact_owned_returns_the_original_allocation_untouched() {
        let plain = String::from("nothing to see");
        assert_eq!(redact_owned(plain), "nothing to see");
        assert_eq!(
            redact_owned("https://x-access-token:ghp_abc@github.com/o/r.git".into()),
            "https://***@github.com/o/r.git"
        );
    }
}
