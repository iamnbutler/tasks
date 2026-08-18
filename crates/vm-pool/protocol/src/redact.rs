//! Credential scrubbing at the point of *formatting*.
//!
//! vm-pool hands a VM its environment, and for an agent VM that environment is
//! where the credential is. Every rendering of that environment — the
//! `container run` argument vector, a serialized [`crate::VmCommand`], a
//! `{:?}` of a config — is a place the value can be written to a log sink and
//! kept forever (#923). So the scrub lives in the `Debug`/`Display` impls the
//! formatter reaches, not at the call sites: a wrapper the caller has to
//! remember to apply is one new field away from the same bug.
//!
//! # This is a deny-list, and it fails *open*
//!
//! [`is_secret_name`] matches a fixed set of name suffixes. **A secret whose
//! name matches none of them is logged in full, silently.** Adding a new
//! credential to any environment this formats means adding its name shape
//! here — nothing else will catch it.
//!
//! Failing open is the right trade for vm-pool specifically: this is generic
//! infrastructure that cannot know its consumers' variables, and the inverse
//! (an allow-list of names known to be safe) would redact every unknown
//! consumer's operational values — `CARGO_BUILD_JOBS`, an image tag, a branch
//! — and turn a diagnostic log line into a row of `<redacted>`. It is stated
//! rather than implied because a security control that fails open and does not
//! say so is one people trust further than it earns.
//!
//! Redaction is **name-based, never value-based**: no pattern reliably tells
//! an API key from a job count, and the name is always kept, because "did the
//! key get through at all" is exactly what the line is read for.
//!
//! One shape the suffix rule structurally cannot catch: a name whose secret
//! word is *present but not terminal*, `AWS_ACCESS_KEY_ID` being the classic —
//! it contains `_KEY` but ends in `_ID`, so the family rule does not fire and
//! the fix is to name it. `SECRET_NAMES` is where such a name goes.
//!
//! # Not a substitute for rotation
//!
//! Scrubbing stops *new* writes. It does nothing about what a previous build
//! already wrote to a console, a redirected stdout or an archived copy of one.
//! A credential that has been logged is exposed until a human rotates it.

use std::borrow::Cow;
use std::fmt;

use serde_json::Value;

/// What a redacted value is replaced with.
///
/// Contains no separator and no boundary character, which is what makes a
/// second scrub of already-scrubbed text a no-op — see the idempotency test.
pub const REDACTED: &str = "<redacted>";

/// Name suffixes that mark a value as a credential.
///
/// Matched case-insensitively at a **word boundary**, so `MONKEY` is not a
/// `_KEY` and `TOKENIZER` is not a `_TOKEN`. See the module docs: this list is
/// the whole control, and a name that is not on it is printed.
const SECRET_SUFFIXES: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PWD",
    "CREDENTIAL",
    "CREDENTIALS",
    "PAT",
    "AUTH",
];

/// Whole names that carry a credential without ending in one of
/// [`SECRET_SUFFIXES`].
///
/// The suffix rule reads the *end* of a name, so a secret word buried in the
/// middle is invisible to it. Matched case-insensitively and in full.
const SECRET_NAMES: &[&str] = &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"];

/// Whether a value named `name` should be masked wherever it is formatted.
///
/// A deny-list on the name — see the module docs for which way it fails, and
/// why that is the trade for infrastructure that cannot know its consumers.
pub fn is_secret_name(name: &str) -> bool {
    let name = name.trim().trim_matches(|c| c == '"' || c == '\'');
    if name.is_empty() {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    if SECRET_NAMES.iter().any(|n| upper == *n) {
        return true;
    }
    SECRET_SUFFIXES.iter().any(|suffix| {
        let Some(head) = upper.strip_suffix(suffix) else {
            return false;
        };
        // `TOKEN` itself, or a `_`/`-`/`.`-separated word ending in one. Not
        // `MONKEY`, whose `KEY` runs on from a letter.
        head.is_empty() || head.ends_with(['_', '-', '.'])
    })
}

/// Characters that end a value. Deliberately **not** `=`, `:` or `,`: half a
/// secret is still a secret, so `TOKEN=aaa:bbb,ccc` is masked whole rather
/// than up to its first separator.
///
/// `<` and `>` are absent on purpose — [`REDACTED`] is built from them, and a
/// boundary inside the replacement would make a second scrub extend it.
const VALUE_BOUNDARY: &[char] = &[
    ' ', '\t', '\n', '\r', '"', '\'', '`', '[', ']', '{', '}', '(', ')',
];

/// Characters that end a URL's authority.
const AUTHORITY_END: &[char] = &['/', '?', '#', ' ', '\t', '\n', '\r', '\'', '"', '`'];

/// Strip the userinfo from every `scheme://` URL in `text`.
///
/// Credential-aware rather than naive: only the authority of a URL is touched,
/// so an `@` in a path and a mail-shaped log line both survive. This is the
/// half that catches the credentialed clone URL
/// (`https://x-access-token:<token>@github.com/…`) riding a wire message.
pub fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        let (before, after) = rest.split_at(idx + 3);
        out.push_str(before);
        let end = after.find(AUTHORITY_END).unwrap_or(after.len());
        let authority = &after[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(REDACTED);
                out.push('@');
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Mask credentials in free text: [`scrub_urls`], then every
/// `NAME <sep> VALUE` pair whose name [`is_secret_name`].
///
/// `sep` is `=`, `:` or `,`. The last two are not decoration: `,` is what a
/// `(String, String)` pair's own `Debug` renders (`("NAME", "value")`), and
/// `:` is what a JSON field renders — so a `Debug` of an environment and a
/// serialized command are both covered without either having to be parsed.
///
/// The URL pass runs **first** so that a `…x-access-token:<token>@host` is
/// already `…<redacted>@host` by the time the pair rule sees it; the other
/// order would mask from the `token:` onwards and take the host with it, and
/// the host is the operational half of the line.
pub fn scrub_text(text: &str) -> String {
    scrub_pairs(&scrub_urls(text))
}

/// The `NAME <sep> VALUE` half of [`scrub_text`], on text whose URLs have
/// already been scrubbed.
fn scrub_pairs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(rel) = text[i..].find(['=', ':', ',']) {
        let sep = i + rel;
        let sep_end = sep + text[sep..].chars().next().map_or(1, char::len_utf8);
        let name = trailing_name(&text[i..sep]);
        out.push_str(&text[i..sep_end]);
        i = sep_end;
        if !is_secret_name(name) {
            continue;
        }
        // Skip the space a `Debug` or a pretty-printer puts after a separator.
        let value_start = i + text[i..].len() - text[i..].trim_start_matches([' ', '\t']).len();
        out.push_str(&text[i..value_start]);
        let rest = &text[value_start..];
        let (value_len, quote) = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => (
                rest[q.len_utf8()..]
                    .find(q)
                    .map_or(rest.len(), |end| end + 2 * q.len_utf8()),
                Some(q),
            ),
            _ => (rest.find(VALUE_BOUNDARY).unwrap_or(rest.len()), None),
        };
        if value_len == 0 {
            i = value_start;
            continue;
        }
        match quote {
            Some(q) => {
                out.push(q);
                out.push_str(REDACTED);
                // A value whose closing quote never arrived: do not invent one.
                if rest[q.len_utf8()..].find(q).is_some() {
                    out.push(q);
                }
            }
            None => out.push_str(REDACTED),
        }
        i = value_start + value_len;
    }
    out.push_str(&text[i..]);
    out
}

/// The name a separator belongs to: the last `[A-Za-z0-9_.-]` run before it,
/// with one layer of quoting removed.
fn trailing_name(head: &str) -> &str {
    let head = head.trim_end();
    let head = head.strip_suffix(['"', '\'']).unwrap_or(head);
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .last()
        .map_or(head.len(), |(idx, _)| idx);
    &head[start..]
}

/// Mask credentials in a JSON document: every object field and every
/// `[name, value]` pair whose name [`is_secret_name`], plus the userinfo of
/// every URL-shaped string.
///
/// The array case is the one that matters here and is easy to miss: an
/// environment is `Vec<(String, String)>`, which serializes as
/// `[[name, value], …]`, so on the wire the name is the value's *sibling* and
/// not its key — a scrubber that only looked at field names would miss
/// precisely the environment it is here for.
///
/// Text that is not JSON falls back to [`scrub_text`], so this is always safe
/// to call on a line that may or may not be a serialized message.
pub fn scrub_json(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(mut value) => {
            scrub_value(&mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| scrub_text(text))
        }
        Err(_) => scrub_text(text),
    }
}

fn scrub_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            let scrubbed = scrub_urls(s);
            if scrubbed != *s {
                *s = scrubbed;
            }
        }
        Value::Array(items) => {
            if let [Value::String(name), rest] = items.as_mut_slice()
                && is_secret_name(name)
            {
                *rest = Value::String(REDACTED.into());
                return;
            }
            for item in items {
                scrub_value(item);
            }
        }
        Value::Object(map) => {
            for (name, field) in map.iter_mut() {
                if is_secret_name(name) {
                    *field = Value::String(REDACTED.into());
                } else {
                    scrub_value(field);
                }
            }
        }
        _ => {}
    }
}

/// Cheap negative check for callers on a hot path: no `://`, no separator and
/// no `@` means the scrubs provably cannot change the text.
pub fn may_contain_secrets(text: &str) -> bool {
    text.contains("://") || text.contains(['=', ':', ','])
}

/// [`scrub_text`] for a borrowed string, allocating only when there is
/// something that could be scrubbed.
pub fn scrub_line(text: &str) -> Cow<'_, str> {
    if may_contain_secrets(text) {
        Cow::Owned(scrub_text(text))
    } else {
        Cow::Borrowed(text)
    }
}

/// A string rendered with its credentials masked, scrubbed **inside** the
/// formatter.
///
/// Two consequences, both deliberate: a disabled log level costs nothing
/// because `Display` is never called, and no unscrubbed `String` is left
/// lying around for the next person to log by accident.
///
/// JSON is parsed and walked ([`scrub_json`]); anything else falls back to the
/// text rules.
pub struct Scrubbed<'a>(pub &'a str);

impl fmt::Display for Scrubbed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&scrub_json(self.0))
    }
}

impl fmt::Debug for Scrubbed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&scrub_json(self.0), f)
    }
}

/// Any `Debug` value rendered with its credentials masked, scrubbed inside the
/// formatter.
///
/// Wraps by reference so `?field` on a value the caller still owns is one
/// character of change: `?event` becomes `?ScrubbedDebug(&event)`.
pub struct ScrubbedDebug<'a, T: fmt::Debug>(pub &'a T);

impl<T: fmt::Debug> fmt::Debug for ScrubbedDebug<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&scrub_text(&format!("{:?}", self.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every fixture below is a value that cannot authenticate anywhere. Do not
    // paste a real credential into this file to demonstrate a leak: the leak is
    // described by its call site, never by its value.
    const FAKE: &str = "not-a-real-credential-0000";

    #[test]
    fn secret_names_match_at_a_word_boundary() {
        for name in [
            "ANTHROPIC_API_KEY",
            "anthropic_api_key",
            "GITHUB_TOKEN",
            "SOME_SECRET",
            "DB_PASSWORD",
            "DB_PASSWD",
            "DB_PWD",
            "REGISTRY_CREDENTIAL",
            "REGISTRY_CREDENTIALS",
            "GH_PAT",
            "PROXY_AUTH",
            "TOKEN",
            "npm-token",
            "aws_access_key_id",
        ] {
            assert!(is_secret_name(name), "{name} should be secret");
        }
        for name in ["MONKEY", "TOKENIZER", "CARGO_BUILD_JOBS", "PATH", "", "  "] {
            assert!(!is_secret_name(name), "{name} should not be secret");
        }
    }

    /// The recorded boundary of a control that fails **open**. A name outside
    /// the deny-list is printed in full, and that is a decision rather than a
    /// gap: see the module docs. If a future name shape needs covering, add it
    /// to `SECRET_SUFFIXES`/`SECRET_NAMES` — do not weaken this test.
    #[test]
    fn an_unmatched_name_is_deliberately_not_redacted() {
        assert!(!is_secret_name("ANTHROPIC_API_SESSION"));
        assert_eq!(
            scrub_text(&format!("ANTHROPIC_API_SESSION={FAKE}")),
            format!("ANTHROPIC_API_SESSION={FAKE}")
        );
    }

    #[test]
    fn env_pairs_are_masked_and_their_names_kept() {
        assert_eq!(
            scrub_text(&format!("ANTHROPIC_API_KEY={FAKE}")),
            "ANTHROPIC_API_KEY=<redacted>"
        );
        assert_eq!(
            scrub_text(&format!("-e ANTHROPIC_API_KEY={FAKE} image:v1")),
            "-e ANTHROPIC_API_KEY=<redacted> image:v1"
        );
        // Operational values are untouched — that is what the line is for.
        assert_eq!(
            scrub_text("CARGO_BUILD_JOBS=3 SCOUT_IMAGE=agent:v1"),
            "CARGO_BUILD_JOBS=3 SCOUT_IMAGE=agent:v1"
        );
    }

    /// Half a secret is still a secret: a value ends at a boundary, never at a
    /// separator inside it.
    #[test]
    fn a_value_containing_separators_is_masked_whole() {
        assert_eq!(scrub_text("TOKEN=aaa:bbb,ccc"), "TOKEN=<redacted>");
        assert_eq!(
            scrub_text("TOKEN=aaa:bbb,ccc rest"),
            "TOKEN=<redacted> rest"
        );
    }

    /// `(String, String)`'s own `Debug` puts the name and the value on either
    /// side of a comma.
    #[test]
    fn debug_rendered_tuples_are_masked() {
        let env = vec![
            ("ANTHROPIC_API_KEY".to_string(), FAKE.to_string()),
            ("CARGO_BUILD_JOBS".to_string(), "3".to_string()),
        ];
        assert_eq!(
            format!("{:?}", ScrubbedDebug(&env)),
            r#"[("ANTHROPIC_API_KEY", "<redacted>"), ("CARGO_BUILD_JOBS", "3")]"#
        );
    }

    #[test]
    fn urls_lose_their_userinfo_and_keep_their_host() {
        assert_eq!(
            scrub_text(&format!(
                "clone https://x-access-token:{FAKE}@github.com/o/r.git"
            )),
            "clone https://<redacted>@github.com/o/r.git"
        );
        // An `@` outside an authority is not a credential.
        assert_eq!(
            scrub_text("committing as test@example.com"),
            "committing as test@example.com"
        );
        assert_eq!(
            scrub_text("https://github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
    }

    /// The shape in the report: an environment is a list of pairs, so the
    /// name is the value's sibling rather than its key.
    #[test]
    fn json_environments_are_masked_through_their_sibling_name() {
        let json = format!(
            r#"{{"type":"allocate","config":{{"env":[["ANTHROPIC_API_KEY","{FAKE}"],["CARGO_BUILD_JOBS","3"]]}}}}"#
        );
        let scrubbed = scrub_json(&json);
        assert!(!scrubbed.contains(FAKE), "{scrubbed}");
        assert!(scrubbed.contains("ANTHROPIC_API_KEY"), "{scrubbed}");
        assert!(scrubbed.contains(r#""3""#), "{scrubbed}");
    }

    #[test]
    fn json_object_fields_are_masked_by_name() {
        let json = format!(r#"{{"github_token":"{FAKE}","branch":"main"}}"#);
        let scrubbed = scrub_json(&json);
        assert!(!scrubbed.contains(FAKE), "{scrubbed}");
        assert!(scrubbed.contains(r#""branch":"main""#), "{scrubbed}");
    }

    #[test]
    fn json_strings_keep_their_urls_minus_the_credential() {
        let json =
            format!(r#"{{"repo_clone_url":"https://x-access-token:{FAKE}@github.com/o/r.git"}}"#);
        let scrubbed = scrub_json(&json);
        assert!(!scrubbed.contains(FAKE), "{scrubbed}");
        assert!(scrubbed.contains("github.com/o/r.git"), "{scrubbed}");
    }

    #[test]
    fn non_json_falls_back_to_the_text_rules() {
        assert_eq!(
            scrub_json(&format!("not json at all GITHUB_TOKEN={FAKE}")),
            "not json at all GITHUB_TOKEN=<redacted>"
        );
    }

    /// Load-bearing rather than tidy: text crosses several hops and is
    /// routinely scrubbed twice. A second pass that extended `<redacted>` or
    /// ate the host after `<redacted>@` would corrupt exactly what it protects.
    #[test]
    fn both_scrubs_are_idempotent() {
        for input in [
            format!("ANTHROPIC_API_KEY={FAKE}"),
            format!("-e GITHUB_TOKEN={FAKE} -e CARGO_BUILD_JOBS=3"),
            format!("https://x-access-token:{FAKE}@github.com/o/r.git"),
            format!(r#"[("ANTHROPIC_API_KEY", "{FAKE}")]"#),
            format!(r#"{{"env":[["ANTHROPIC_API_KEY","{FAKE}"]]}}"#),
            "no secrets here at all".to_string(),
            // Already scrubbed by the tasks-side redactor, which masks with
            // `***@` — a different implementation this must not corrupt.
            "https://***@github.com/o/r.git".to_string(),
        ] {
            let once = scrub_text(&input);
            assert_eq!(
                scrub_text(&once),
                once,
                "text: second pass changed {once:?}"
            );
            let once = scrub_json(&input);
            assert_eq!(
                scrub_json(&once),
                once,
                "json: second pass changed {once:?}"
            );
        }
    }

    #[test]
    fn scrubbing_happens_inside_the_formatter() {
        let line = format!(r#"{{"env":[["ANTHROPIC_API_KEY","{FAKE}"]]}}"#);
        let rendered = format!("{}", Scrubbed(&line));
        assert!(!rendered.contains(FAKE), "{rendered}");
        assert!(rendered.contains("ANTHROPIC_API_KEY"), "{rendered}");
        // The wrapper borrows: the original is untouched, because redaction is
        // a property of formatting and never of the data.
        assert!(line.contains(FAKE));
    }

    #[test]
    fn scrub_line_borrows_when_there_is_nothing_to_scrub() {
        assert!(matches!(scrub_line("plain agent output"), Cow::Borrowed(_)));
        assert_eq!(
            scrub_line(&format!("GITHUB_TOKEN={FAKE}")),
            "GITHUB_TOKEN=<redacted>"
        );
    }
}
