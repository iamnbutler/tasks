//! What the Credentials surface says about each sealed name (#1005).
//!
//! One row per name in [`SecretName::ALL`], read from `GET /secrets`. Like
//! [`crate::empty_state`], [`crate::feed`] and [`crate::chat_log`] this module
//! is deliberately **gpui-free** — pure functions over plain data, unit-tested
//! under `make app-test`. `app-gpui` is not a workspace member, so `make test`
//! compiles none of it, and what keeps these states honest is that every one
//! of them is decided here by a function a test can call rather than by a
//! condition spelled out at a render site nothing can run.
//!
//! ## Three states, not two
//!
//! [`tasks_client::api::http::SecretSource`] has **four** variants and the
//! distinction that matters is that `ApiKeyHelper` is a third way a key can
//! already be serving: Claude Code's own helper on the host, Anthropic-only.
//! Collapsing it into "environment" tells a human to paste a key they do not
//! need; collapsing it into "unconfigured" is worse, because it says a working
//! install is broken.
//!
//! ## What a missing value costs, stated on the row
//!
//! [`KeyRow::consequence`] is per state and is the sentence the issue asks to
//! be inline: removing a sealed key whose environment variable is also set
//! says the variable serves again, and removing one with no fallback says what
//! actually stops. A destructive control whose consequence is a paragraph
//! somewhere else is one people press without reading.
//!
//! ## The degraded states are derived here, and not read off `/status`
//!
//! The server reports a missing credential as a startup `warn!` and as
//! per-route 503s; nothing over the API says "polling is disabled". So the
//! banner is derived from `GET /secrets` — which is the better shape anyway:
//! one observation, one place, and the paste that fixes it is in the same
//! pane. **Do not add a credential field to `ServerStatus`.**

use chrono::{DateTime, Utc};
use tasks_client::api::http::{SecretSource, SecretsStatus};
use tasks_client::api::models::SecretName;

/// How one name is resolving, as the server last reported it.
///
/// Four states rather than a `bool`, and the three non-`Unconfigured` ones are
/// all *serving*: a pipeline whose GitHub token comes from the environment
/// polls, writes and fills every list below it. That is why only
/// [`KeyState::Unconfigured`] may ever diagnose an empty pane — see
/// [`crate::empty_state::Situation::NoCredentials`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    /// In the sealed store, and the store is what serves it. What production
    /// should say.
    Sealed { at: Option<DateTime<Utc>> },
    /// A boot-captured environment variable is serving. It works; sealing it
    /// is the nudge, not an emergency.
    EnvironmentOnly { var: String },
    /// Claude Code's `apiKeyHelper` on the host is serving. Anthropic only.
    HelperOnly { path: String },
    /// Nothing resolves this name at all.
    Unconfigured,
}

impl KeyState {
    /// Read one entry, or `Unconfigured` when the status carries no row for
    /// this name.
    ///
    /// An absent row is "nothing sealed", never a missing row — the server
    /// promises one entry per name, and left-joining here means a server that
    /// stops promising it degrades to the same answer rather than to a panic
    /// or a blank.
    pub fn read(status: &SecretsStatus, name: SecretName) -> Self {
        let Some(entry) = status.entry(name) else {
            return KeyState::Unconfigured;
        };
        match &entry.serving {
            SecretSource::Sealed => KeyState::Sealed { at: entry.set_at },
            SecretSource::Environment { var } => KeyState::EnvironmentOnly { var: var.clone() },
            SecretSource::ApiKeyHelper { path } => KeyState::HelperOnly { path: path.clone() },
            // Sealed but not serving is possible — the unseal key moved — and
            // it reads as unconfigured on purpose: what matters to every
            // reader of this state is whether the credential *works*.
            SecretSource::Unset => KeyState::Unconfigured,
        }
    }

    /// Whether something is serving this name right now.
    pub fn is_serving(&self) -> bool {
        !matches!(self, KeyState::Unconfigured)
    }

    /// Whether what serves it is a fallback rather than the store — the nudge,
    /// and nothing louder.
    pub fn is_fallback(&self) -> bool {
        matches!(
            self,
            KeyState::EnvironmentOnly { .. } | KeyState::HelperOnly { .. }
        )
    }
}

/// One row of the Credentials pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub name: SecretName,
    pub state: KeyState,
    /// What this key is for, in one clause.
    pub purpose: &'static str,
    /// Where it is coming from, for the line under the title.
    pub detail: String,
    /// What removing it would do, stated inline rather than in a dialog.
    /// Empty when there is nothing to remove.
    pub consequence: String,
    /// The nudge, when something other than the store is serving.
    pub degraded: Option<String>,
}

impl KeyRow {
    /// Whether this row offers a Remove control at all.
    pub fn removable(&self) -> bool {
        matches!(self.state, KeyState::Sealed { .. })
    }
}

/// The pane's rows — always one per name, in the order [`SecretName::ALL`]
/// declares them, whatever the server sent.
///
/// `None` is "not observed yet" and produces the same rows in
/// [`KeyState::Unconfigured`]; a placement that must not accuse on absence
/// reads [`crate::empty_state`]'s `Option<KeyState>` instead, which is `None`
/// until the first answer arrives.
pub fn rows(status: Option<&SecretsStatus>) -> Vec<KeyRow> {
    SecretName::ALL
        .into_iter()
        .map(|name| {
            let state = status
                .map(|status| KeyState::read(status, name))
                .unwrap_or(KeyState::Unconfigured);
            KeyRow {
                name,
                purpose: purpose(name),
                detail: detail(name, &state),
                consequence: consequence(name, &state),
                degraded: degraded(name, &state),
                state,
            }
        })
        .collect()
}

fn purpose(name: SecretName) -> &'static str {
    match name {
        SecretName::AnthropicApiKey => {
            "Every Scout, Builder and worker redeems this through the broker."
        }
        SecretName::GithubToken => {
            "Issue polling, every GitHub write, and the clone inside each VM."
        }
    }
}

fn detail(name: SecretName, state: &KeyState) -> String {
    match state {
        KeyState::Sealed { at: Some(at) } => {
            format!("Sealed {}", at.format("%Y-%m-%d %H:%M UTC"))
        }
        KeyState::Sealed { at: None } => "Sealed".to_string(),
        KeyState::EnvironmentOnly { var } => format!("Serving from {var} in the environment"),
        KeyState::HelperOnly { path } => format!("Serving from Claude Code's apiKeyHelper ({path})"),
        // Says "not in the sealed store" and not "not configured": on a
        // server whose `serving` we could not read, an environment fallback
        // may well be working, and claiming otherwise is the one wrong
        // sentence here.
        KeyState::Unconfigured => format!("Not in the sealed store ({name})"),
    }
}

/// What removing this would do. Per state, because the answer genuinely
/// differs: with a fallback set it degrades, without one it stops.
fn consequence(name: SecretName, state: &KeyState) -> String {
    if !matches!(state, KeyState::Sealed { .. }) {
        return String::new();
    }
    match name {
        SecretName::AnthropicApiKey => format!(
            "Removing it falls back to {} or Claude Code's apiKeyHelper if either is \
             set on the server; with neither, agents cannot authenticate and every \
             run fails at its first API call.",
            name.env_var()
        ),
        SecretName::GithubToken => format!(
            "Removing it falls back to {} if that is set on the server; with neither, \
             GitHub polling and every GitHub write stop.",
            name.env_var()
        ),
    }
}

/// The nudge. Said on the row where the paste that fixes it is — and never
/// worded as a failure, because a fallback that is serving is serving.
fn degraded(name: SecretName, state: &KeyState) -> Option<String> {
    match state {
        KeyState::EnvironmentOnly { var } => Some(format!(
            "{var} in the server's environment is what serves this today. It works, \
             and it is boot-captured — paste the value here to seal it, and a restart \
             stops changing what the pipeline authenticates as."
        )),
        KeyState::HelperOnly { path } => Some(format!(
            "Claude Code's apiKeyHelper ({path}) on this host is what serves this \
             today. Nothing is broken; sealing it makes the server's credential its \
             own rather than the host user's."
        )),
        KeyState::Unconfigured => Some(match name {
            SecretName::AnthropicApiKey => {
                "Nothing resolves this, so every Scout, Builder and worker fails at \
                 its first API call."
                    .to_string()
            }
            SecretName::GithubToken => {
                "Nothing resolves this, so no issue is ingested and every GitHub \
                 write fails."
                    .to_string()
            }
        }),
        KeyState::Sealed { .. } => None,
    }
}

/// Where the store lives and what holds its unseal key — the one line above
/// the rows.
///
/// Silent-ish rather than alarming when nothing has been observed: "not
/// observed" is not "not configured", the `images: Vec<ImageIdentity>` rule
/// one surface over.
pub fn store_line(status: Option<&SecretsStatus>) -> String {
    let Some(status) = status else {
        return "The server has not answered about its credential store yet.".to_string();
    };
    if !status.initialized {
        return format!(
            "No sealed store at {} yet — pasting a value here creates one.",
            status.store_path
        );
    }
    match &status.key_source {
        Some(source) => format!("Sealed store at {} — unseal key: {source}", status.store_path),
        None => format!("Sealed store at {}", status.store_path),
    }
}

/// Whether the submit control is live. Mirrors the server's 400 on an empty
/// value, so the refusal is not a round trip.
pub fn submittable(len_chars: usize) -> bool {
    len_chars > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_client::api::http::SecretEntry;

    /// Obviously-synthetic throughout: nothing here is or resembles a
    /// credential, per the standing rule that no fragment of a real one goes
    /// in code, tests or a PR body.
    fn status(entries: Vec<SecretEntry>) -> SecretsStatus {
        SecretsStatus {
            store_path: "/tmp/example/secrets/sealed.json".into(),
            initialized: true,
            key_source: Some("the login keychain".into()),
            entries,
        }
    }

    fn entry(name: SecretName, serving: SecretSource, sealed: bool) -> SecretEntry {
        SecretEntry {
            name,
            set_at: sealed.then(|| DateTime::from_timestamp(1_800_000_000, 0).unwrap()),
            serving,
        }
    }

    /// Both rows always render, in declaration order, whatever the server
    /// sent — an absent name is "nothing sealed", never a missing row.
    #[test]
    fn both_names_always_have_a_row() {
        let only_github = status(vec![entry(
            SecretName::GithubToken,
            SecretSource::Sealed,
            true,
        )]);
        let listed = rows(Some(&only_github));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, SecretName::AnthropicApiKey);
        assert_eq!(listed[0].state, KeyState::Unconfigured);
        assert!(matches!(listed[1].state, KeyState::Sealed { .. }));

        // And with nothing observed at all.
        assert_eq!(rows(None).len(), 2);
    }

    /// The three-variant finding: a helper-served Anthropic key is neither
    /// "environment" nor "unconfigured". Collapsing it either way prompts for
    /// a paste that is not needed, on a working install.
    #[test]
    fn a_helper_served_key_is_its_own_state_and_is_serving() {
        let s = status(vec![entry(
            SecretName::AnthropicApiKey,
            SecretSource::ApiKeyHelper {
                path: "/usr/local/bin/example-helper".into(),
            },
            false,
        )]);
        let state = KeyState::read(&s, SecretName::AnthropicApiKey);
        assert!(matches!(state, KeyState::HelperOnly { .. }));
        assert!(state.is_serving(), "a helper-served key works");
        assert!(state.is_fallback(), "…and is still worth sealing");

        let row = &rows(Some(&s))[0];
        assert!(row.degraded.as_deref().unwrap().contains("Nothing is broken"));
        assert!(!row.removable(), "there is nothing sealed to remove");
    }

    /// An environment-served key is serving, and its nudge says so rather
    /// than reporting a failure.
    #[test]
    fn an_environment_served_key_nudges_and_does_not_accuse() {
        let s = status(vec![entry(
            SecretName::GithubToken,
            SecretSource::Environment {
                var: "GITHUB_TOKEN".into(),
            },
            false,
        )]);
        let row = &rows(Some(&s))[1];
        assert_eq!(
            row.state,
            KeyState::EnvironmentOnly {
                var: "GITHUB_TOKEN".into()
            }
        );
        assert!(row.degraded.as_deref().unwrap().contains("It works"));
        assert!(row.detail.contains("Serving from GITHUB_TOKEN"));
    }

    /// A sealed key has no nudge, offers Remove, and states what removing it
    /// costs — inline, where the control is.
    #[test]
    fn a_sealed_key_states_what_removing_it_costs() {
        let s = status(vec![
            entry(SecretName::AnthropicApiKey, SecretSource::Sealed, true),
            entry(SecretName::GithubToken, SecretSource::Sealed, true),
        ]);
        let listed = rows(Some(&s));
        for row in &listed {
            assert!(row.degraded.is_none());
            assert!(row.removable());
            assert!(row.consequence.contains(row.name.env_var()));
        }
        assert!(listed[1].consequence.contains("GitHub polling"));
        assert!(listed[0].consequence.contains("first API call"));
        assert!(listed[0].detail.starts_with("Sealed "));
    }

    /// "Not in the sealed store", never "not configured" — an environment
    /// fallback may be serving on a server whose `serving` we could not read,
    /// and claiming otherwise is the one wrong sentence here.
    #[test]
    fn an_unconfigured_row_says_not_in_the_sealed_store() {
        let row = &rows(None)[1];
        assert!(row.detail.contains("Not in the sealed store"));
        assert!(!row.detail.contains("not configured"));
        assert!(!row.removable());
    }

    /// The header line distinguishes "no store yet", "not observed" and a
    /// store that exists — three different sentences for three different
    /// facts.
    #[test]
    fn the_store_line_separates_absent_unobserved_and_present() {
        assert!(store_line(None).contains("has not answered"));

        let mut fresh = status(vec![]);
        fresh.initialized = false;
        assert!(store_line(Some(&fresh)).contains("creates one"));

        assert!(store_line(Some(&status(vec![]))).contains("login keychain"));
    }

    /// The client-side refusal mirrors the server's 400, so an empty submit
    /// is not a round trip.
    #[test]
    fn an_empty_buffer_is_not_submittable() {
        assert!(!submittable(0));
        assert!(submittable(1));
    }
}
