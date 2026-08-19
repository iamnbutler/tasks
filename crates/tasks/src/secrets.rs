//! Custody of the two upstream credentials, so that no raw key ever sits in
//! `.env`, in the process environment, in argv, or in anything a VM receives.
//!
//! See `docs/plans/2026-08-18-credential-custody.md` for the whole design.
//! This module is the host half: **sealed storage** plus the [`Secrets`]
//! handle the server reads keys through. The other half — short-lived leases
//! and the broker that spends these keys on a VM's behalf — is
//! [`crate::broker`], which is the *only* consumer that sends a raw key
//! anywhere, and only ever over TLS to the real upstream.
//!
//! # The sealed store, and the two-key property
//!
//! Raw values live in `<data dir>/secrets/sealed.json`, each entry encrypted
//! with ChaCha20-Poly1305 under a key derived (HKDF-SHA256, per-store salt,
//! entry name as AAD) from a 32-byte **unseal key** that is deliberately not
//! stored next to the ciphertext: by default it lives in the macOS Keychain
//! (service `tasks-v2-secrets`), or in a file named by
//! `TASKS_SECRETS_KEY_FILE` where there is no Keychain (Linux, tests). Either
//! artifact alone — a copied data dir, or a copied Keychain entry — decrypts
//! nothing. This is the "two key decrypt" the design asks for, stated
//! honestly: it protects the *at-rest* copies (backups, synced folders, a
//! bundled data dir), not against an attacker already running code as this
//! user, who could read the Keychain the same way the server does.
//!
//! # Where the unseal key lives, and why the key file stays first-class
//!
//! Custody goes through the [`keyring`] crate's native backends —
//! Security.framework on macOS, the Credential Manager on Windows, the Secret
//! Service elsewhere — rather than through `/usr/bin/security` (#1003).
//! [`keychain_read`] and [`keychain_write`] are the entire boundary.
//!
//! What that does **not** buy, stated here because the change reads like it
//! does. An item created by the `security` CLI keeps that command's access
//! list through any number of native writes: `set_password` on macOS is
//! find-then-modify-in-place, so rewriting rebinds nothing. The legacy
//! `security` read therefore stays on the read path as the default fallback,
//! which is what keeps existing installs working — and means custody for one
//! of them is **unchanged** until a human runs `tasks secrets rehome-key`.
//! Nothing forces that; a `warn!` is the only prompt.
//!
//! And on a Mac an access list is a decision about an *application*, so an
//! unsigned development build is a different application on every `cargo
//! build` — a natively-created item re-prompts each rebuild, and a server
//! started by launchd with no window server to show the prompt cannot answer
//! it at all. That is why `--key-file` / `TASKS_SECRETS_KEY_FILE` is a
//! first-class way to run this system and not a fallback for exotic hosts.
//! The real improvement arrives with a signed, stable application identity
//! (#988, undecided).
//!
//! One asymmetry is known and **not** detected: the `security` CLI writes the
//! *default* keychain (a user preference, `security default-keychain -s`)
//! while the native backend reads the user domain's default. Normally the same
//! `login.keychain-db`. Where they differ nothing reports it — `status` reads
//! the store header, which says `keychain` either way, and it is documented as
//! needing no unseal key so it cannot probe one. The symptom is a
//! [`rehome_key`] whose delete reports no entry while its write appears to
//! succeed; its read-back comparison is what turns that into a refusal
//! instead of a lost key.
//!
//! # Rotation without a restart
//!
//! The running server reads through [`Secrets`], which re-checks the sealed
//! file's mtime on access and re-decrypts when it changes — so
//! `tasks secrets set github-token` takes effect on the next GitHub poll and
//! the next brokered request, with nothing restarted. That is what makes the
//! #971 rotation a paste rather than a deploy.
//!
//! # In memory
//!
//! Decrypted values cross module boundaries only as [`Secret`]: `Debug`
//! prints `<redacted>`, there is no `Display` at all (interpolating a secret
//! is a compile error, not a silent redaction), equality is constant-time,
//! and the buffer is wiped on drop — so the accident #923 documents (a value
//! riding a formatter into a log sink) cannot recur for anything typed
//! correctly. The deliberate way out is [`Secret::expose`], which is
//! greppable.
//!
//! # Fallbacks, and what refuses to boot
//!
//! Per key, resolution is: sealed store (live) → the process environment as
//! captured at boot (`ANTHROPIC_API_KEY` / `GITHUB_TOKEN`, kept as the
//! dev/test path) → for the Anthropic key only, the host's Claude Code
//! `apiKeyHelper` (`~/.claude/anthropic_key.sh`), as before. But a sealed
//! store that *exists* and cannot be opened — unseal key missing, file
//! corrupt — is a **hard boot error**, not a fallback: an operator who sealed
//! their keys has said the environment is not the source of truth, and a
//! server that silently came up without them would be the ".env silently
//! reverted" failure all over again.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, RwLock};
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use chrono::{DateTime, Utc};
use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::warn;
use zeroize::{Zeroize, Zeroizing};

use crate::redact::Secret;

/// Domain separation for the KEK derivation. Versioned with the store format:
/// a future format bump changes the info string too, so a v2 file can never be
/// silently decrypted with v1 semantics.
const HKDF_INFO: &[u8] = b"tasks-secrets-v1 kek";
const STORE_VERSION: u32 = 1;
/// Credential-store coordinates for the unseal key. The service name carries
/// the project so `security find-generic-password -s tasks-v2-secrets` is
/// self-describing in Keychain Access.
///
/// [`keychain_read`] and [`keychain_write`] are the **whole** custody
/// boundary: they are the only two functions in this repository that touch an
/// OS key store, and everything else — [`init`], [`rehome_key`], the server's
/// [`Secrets::open`] — goes through them. Anything that needs the unseal key
/// (an API route that auto-initialises a store, say) calls one of them; a
/// second key-store path is how the two ends of this come to disagree about
/// which item, which store, and which access list.
const KEYCHAIN_SERVICE: &str = "tasks-v2-secrets";
const KEYCHAIN_ACCOUNT: &str = "unseal";
/// Overrides where the unseal key is read from, whatever the store header
/// says. This is the Linux/test/CI path, and the escape hatch when a Keychain
/// exists but is not usable (a headless Mac with a locked login keychain).
pub const KEY_FILE_ENV: &str = "TASKS_SECRETS_KEY_FILE";

const UNSEAL_KEY_BYTES: usize = 32;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 12;

/// The closed set of secrets this system holds. Closed on purpose: an open
/// namespace would invite values nothing reads, and the broker's scopes and
/// the redaction deny-list are both written against these two names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SecretName {
    AnthropicApiKey,
    GithubToken,
}

impl SecretName {
    pub const ALL: [SecretName; 2] = [SecretName::AnthropicApiKey, SecretName::GithubToken];

    /// The store-file / CLI spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SecretName::AnthropicApiKey => "anthropic-api-key",
            SecretName::GithubToken => "github-token",
        }
    }

    /// The environment variable this entry supersedes.
    pub fn env_var(self) -> &'static str {
        match self {
            SecretName::AnthropicApiKey => "ANTHROPIC_API_KEY",
            SecretName::GithubToken => "GITHUB_TOKEN",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|n| n.as_str() == raw)
    }
}

impl std::fmt::Display for SecretName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("sealed store io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sealed store at {path} is unreadable: {reason}")]
    Malformed { path: PathBuf, reason: String },
    #[error("sealed store version {found} is newer than this binary understands ({STORE_VERSION})")]
    Version { found: u32 },
    #[error("no sealed store at {0} — run `tasks secrets init` first")]
    NotInitialized(PathBuf),
    #[error("a sealed store already exists at {0}")]
    AlreadyInitialized(PathBuf),
    #[error("unseal key unavailable: {0}")]
    Key(String),
    #[error("could not decrypt `{name}` — wrong unseal key, or the file was edited by hand")]
    Decrypt { name: String },
}

/// Where the unseal key lives, as recorded in the store header so the server
/// knows where to look without configuration. `TASKS_SECRETS_KEY_FILE` in the
/// real environment outranks it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum KeySource {
    Keychain,
    File(PathBuf),
}

#[derive(Serialize, Deserialize)]
struct SealedEntry {
    /// Base64, 12 bytes, freshly random per write.
    nonce: String,
    /// Base64 ChaCha20-Poly1305 ciphertext; AAD is the entry name, so a
    /// ciphertext pasted under a different key in the JSON fails to open
    /// rather than answering as the wrong secret.
    ciphertext: String,
    set_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct SealedFile {
    version: u32,
    key_source: KeySource,
    /// Base64, 16 bytes, fixed at init: the HKDF salt binding the KEK to this
    /// store, so the same unseal key used for two stores derives two KEKs.
    salt: String,
    entries: std::collections::BTreeMap<String, SealedEntry>,
}

/// One entry's public metadata, for `tasks secrets status`. Never the value.
pub struct EntryStatus {
    pub name: SecretName,
    pub set_at: DateTime<Utc>,
}

pub struct StoreStatus {
    pub path: PathBuf,
    pub key_source: String,
    pub entries: Vec<EntryStatus>,
}

/// The sealed store's location under a data dir. A subdirectory rather than a
/// bare file so its permissions can be tightened as a unit.
pub fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("secrets").join("sealed.json")
}

fn derive_kek(unseal: &[u8], salt: &[u8]) -> Key {
    let hk = Hkdf::<Sha256>::new(Some(salt), unseal);
    let mut kek = [0u8; 32];
    hk.expand(HKDF_INFO, &mut kek)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    let key = Key::from(kek);
    kek.zeroize();
    key
}

fn parse_file(path: &Path, bytes: &[u8]) -> Result<SealedFile, SecretsError> {
    let file: SealedFile = serde_json::from_slice(bytes).map_err(|e| SecretsError::Malformed {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if file.version > STORE_VERSION {
        return Err(SecretsError::Version {
            found: file.version,
        });
    }
    Ok(file)
}

fn b64_field(path: &Path, field: &str, value: &str) -> Result<Vec<u8>, SecretsError> {
    B64.decode(value).map_err(|e| SecretsError::Malformed {
        path: path.to_path_buf(),
        reason: format!("{field}: {e}"),
    })
}

fn decrypt_entry(
    kek: &Key,
    path: &Path,
    name: &str,
    entry: &SealedEntry,
) -> Result<Secret, SecretsError> {
    let nonce = b64_field(path, "nonce", &entry.nonce)?;
    let ciphertext = b64_field(path, "ciphertext", &entry.ciphertext)?;
    let cipher = ChaCha20Poly1305::new(kek);
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: name.as_bytes(),
            },
        )
        .map_err(|_| SecretsError::Decrypt {
            name: name.to_string(),
        })?;
    let value = String::from_utf8(plaintext).map_err(|_| SecretsError::Decrypt {
        name: name.to_string(),
    })?;
    Ok(Secret::new(value))
}

fn encrypt_entry(kek: &Key, name: &str, value: &str) -> SealedEntry {
    let mut nonce = [0u8; NONCE_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let cipher = ChaCha20Poly1305::new(kek);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: value.as_bytes(),
                aad: name.as_bytes(),
            },
        )
        .expect("ChaCha20-Poly1305 encryption is infallible for in-memory buffers");
    SealedEntry {
        nonce: B64.encode(nonce),
        ciphertext: B64.encode(ciphertext),
        set_at: Utc::now(),
    }
}

// ---------------------------------------------------------------------------
// Unseal key custody
// ---------------------------------------------------------------------------

/// Where the unseal key will actually be read from — the
/// `TASKS_SECRETS_KEY_FILE` override ahead of the source recorded in the
/// store header.
///
/// One decision with two readers: [`resolve_unseal_key`] opens the store with
/// it and [`status`] reports it. Deciding this twice is how `status` came to
/// answer "Keychain" while the override was what opened the store — wrong in
/// exactly the situation the override exists for, since an operator reaches
/// for it when the Keychain is what is failing and reads `status` to confirm
/// it took.
enum KeyLocation {
    Override(PathBuf),
    Keychain,
    File(PathBuf),
}

fn key_location(file: &SealedFile) -> KeyLocation {
    key_location_with(file, std::env::var(KEY_FILE_ENV).ok())
}

/// The decision itself, with the override passed in rather than read — so it
/// is testable without racing every other test through `set_var`, the same
/// reason `updates::pending` splits its environment read out.
fn key_location_with(file: &SealedFile, override_path: Option<String>) -> KeyLocation {
    if let Some(path) = override_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        return KeyLocation::Override(PathBuf::from(path));
    }
    match &file.key_source {
        KeySource::Keychain => KeyLocation::Keychain,
        KeySource::File(path) => KeyLocation::File(path.clone()),
    }
}

impl std::fmt::Display for KeyLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Named as an override, because the difference between this and
            // the line below is the whole reason someone is reading it.
            Self::Override(p) => {
                write!(f, "key file {} (from {KEY_FILE_ENV})", p.display())
            }
            Self::Keychain => write!(f, "Keychain (service `{KEYCHAIN_SERVICE}`)"),
            Self::File(p) => write!(f, "key file {}", p.display()),
        }
    }
}

/// Read the unseal key for `file`, honouring the `TASKS_SECRETS_KEY_FILE`
/// override ahead of the recorded source.
fn resolve_unseal_key(file: &SealedFile) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
    match key_location(file) {
        KeyLocation::Override(path) | KeyLocation::File(path) => read_key_file(&path),
        KeyLocation::Keychain => keychain_read(),
    }
}

fn decode_key_hex(raw: &str, what: &str) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
    let trimmed = raw.trim();
    let mut bytes = Vec::with_capacity(UNSEAL_KEY_BYTES);
    if trimmed.len() != UNSEAL_KEY_BYTES * 2 {
        return Err(SecretsError::Key(format!(
            "{what} holds {} characters, expected {} hex digits",
            trimmed.len(),
            UNSEAL_KEY_BYTES * 2
        )));
    }
    for i in (0..trimmed.len()).step_by(2) {
        let byte = u8::from_str_radix(&trimmed[i..i + 2], 16)
            .map_err(|_| SecretsError::Key(format!("{what} is not hex")))?;
        bytes.push(byte);
    }
    Ok(Zeroizing::new(bytes))
}

fn read_key_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| SecretsError::Key(format!("key file {}: {e}", path.display())))?;
    decode_key_hex(&raw, "the key file")
}

/// Turn a credential-store failure into a [`SecretsError`] that names a way
/// out. Pure, and separate from the calls that produce these errors, so its
/// wording is testable on a host with no credential store at all — which is
/// every host `make test` runs on.
///
/// The way out is always the same two moves, because they are the only two
/// that exist: create the item (`tasks secrets init`), or stop reading the
/// item at all ([`KEY_FILE_ENV`]). A backend error with neither named is how
/// an operator ends up debugging a Keychain instead of setting a variable.
fn keyring_error(op: &str, err: &keyring::Error) -> SecretsError {
    match err {
        keyring::Error::NoEntry => SecretsError::Key(format!(
            "this host's credential store has no `{KEYCHAIN_SERVICE}` item — \
             run `tasks secrets init`, or set {KEY_FILE_ENV} to a key file"
        )),
        // Everything else carries the backend's own words: `NoDefaultStore`
        // (no Security.framework, no Credential Manager, no Secret Service —
        // a headless Linux box), a locked keychain, an access list that
        // refuses this binary. None of them is actionable from here, and all
        // of them are survivable by the same variable.
        other => SecretsError::Key(format!(
            "could not {op} the `{KEYCHAIN_SERVICE}` item in this host's credential \
             store: {other}; run `tasks secrets init`, or set {KEY_FILE_ENV} to a key file"
        )),
    }
}

/// The entry the unseal key lives in. One function, so the platform choice
/// (Security.framework, Credential Manager, Secret Service) is the
/// dependency's and the `cfg` count in this module stays zero.
fn keychain_entry() -> Result<keyring::Entry, SecretsError> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).map_err(|e| keyring_error("open", &e))
}

/// The unseal key as the native store holds it: hex, still encoded.
fn native_key_read() -> Result<Zeroizing<String>, SecretsError> {
    keychain_entry()?
        .get_password()
        .map(Zeroizing::new)
        .map_err(|e| keyring_error("read", &e))
}

/// Read the unseal key out of the OS credential store, falling back to the
/// pre-#1003 `/usr/bin/security` read.
///
/// The fallback is the **default** path and not a contingency: an item created
/// by `security add-generic-password` carries that command's access list, and
/// whether a *different* binary may then decrypt it silently, with a prompt,
/// or not at all is a question only a Mac answers. So the migration read is
/// the one that ships, and an operator moves custody deliberately with
/// [`rehome_key`] — never this path, which must not write (see its own doc).
fn keychain_read() -> Result<Zeroizing<Vec<u8>>, SecretsError> {
    match native_key_read() {
        Ok(hex) => decode_key_hex(&hex, "the credential store item"),
        Err(native) => match legacy_security_read() {
            Some(hex) => {
                LEGACY_READ_WARNED.call_once(|| {
                    warn!(
                        reason = %native,
                        "unseal key read through /usr/bin/security, not the native \
                         credential store; `tasks secrets rehome-key` moves it"
                    );
                });
                decode_key_hex(&hex, "the Keychain item")
            }
            // The native error, not the subprocess's: a message about a
            // missing `/usr/bin/security` is noise on every host that never
            // had one.
            None => Err(native),
        },
    }
}

/// Warned once per process, not per read: the late-unlock guard already
/// bounds the server to one credential-store read ever, and this keeps a CLI
/// that reads twice (`rehome-key`) from saying it twice.
static LEGACY_READ_WARNED: Once = Once::new();

/// The pre-#1003 read: `security find-generic-password -s … -a … -w`.
///
/// Kept **read-only** — there is no `security` write left in this module — and
/// deliberately *not* `cfg`-gated. It is an ordinary subprocess spawn that
/// compiles everywhere and answers `None` where there is no such binary, so
/// the migration path is built and type-checked by every platform rather than
/// only by the one that can exercise it.
fn legacy_security_read() -> Option<Zeroizing<String>> {
    let out = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let hex = Zeroizing::new(String::from_utf8_lossy(&out.stdout).into_owned());
    if hex.trim().is_empty() {
        return None;
    }
    Some(hex)
}

/// Write the unseal key into the OS credential store.
///
/// The hex is a *value* here, which is what the old `security -i`-on-stdin
/// dance existed to achieve: `add-generic-password -w <hex>` as an ordinary
/// argument would have put the key in argv, the exposure class this module
/// exists to close. Through the native API it is free.
fn keychain_write(hex: &str) -> Result<(), SecretsError> {
    keychain_entry()?
        .set_password(hex)
        .map_err(|e| keyring_error("write", &e))
}

/// Where a [`rehome_key`] parks the only copy of the unseal key for the
/// duration of the delete-then-add, and permanently if that window fails.
///
/// **Not under `<data dir>/secrets/`, and not under the data dir at all.** The
/// whole property this module rests on is that the sealed store and the unseal
/// key are two artifacts and neither alone decrypts anything; a rescue file
/// beside `sealed.json` would put both halves in one `cp -r`, one backup
/// sweep, one `tar` — exactly when the file is deliberately being kept,
/// because the rehome failed. `$HOME/.tasks/` is the service home #1012
/// established (the installed binary lives at `~/.tasks/bin/tasks`), it is
/// per-user, and it is not what anyone backs up when they back up the
/// server's state. `write_private` tightens it to 0700 on the way past.
fn rescue_path() -> Result<PathBuf, SecretsError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty())
        .ok_or_else(|| {
            SecretsError::Key(format!(
                "HOME is not set, so there is nowhere outside the sealed store's own \
                 directory to park a rescue copy of the unseal key; \
                 re-home from a session that has one, or set {KEY_FILE_ENV}"
            ))
        })?;
    Ok(home
        .join(crate::service::HOME_DIR)
        .join("unseal.key.rescue"))
}

/// The sentence every failure after the delete carries. It has to say the
/// path, and it has to say the file is a live copy of the key that must not
/// be left lying around.
fn rescue_hint(rescue: &Path) -> String {
    format!(
        "the unseal key is still at {} — the store opens with \
         `{KEY_FILE_ENV}={}`, and that file must be deleted once custody is settled \
         again (it is the other half of the two-artifact property)",
        rescue.display(),
        rescue.display()
    )
}

/// Recreate the unseal-key item through the native API, so *this* binary's
/// access list governs it rather than `/usr/bin/security`'s.
///
/// This is a separate operator command and not something the read path does,
/// for a platform reason rather than a taste one: `set_password` on macOS is
/// find-then-`SecKeychainItemModifyAttributesAndData` — an **in-place**
/// modify — so rewriting a CLI-created item through `keyring` rebinds nothing
/// and is a silent no-op. Only delete-then-add moves the access list, that is
/// destructive, and a delete that lands with an add that does not takes the
/// only copy of the key with it.
///
/// Hence the order, which is the design: read the key, park it in a rescue
/// file *outside the data dir* ([`rescue_path`]), delete, add, **read back and
/// compare**, prove the store still opens, and only then remove the rescue
/// file. Every failure from the delete onwards returns [`rescue_hint`]. The
/// key itself is never printed, returned or logged.
pub fn rehome_key(data_dir: &Path) -> Result<String, SecretsError> {
    let path = store_path(data_dir);
    let file = read_store(&path)?;
    match key_location(&file) {
        KeyLocation::Keychain => {}
        KeyLocation::File(p) => {
            return Err(SecretsError::Key(format!(
                "this store's unseal key is a file ({}), not a credential-store item — \
                 there is nothing to re-home",
                p.display()
            )));
        }
        KeyLocation::Override(p) => {
            return Err(SecretsError::Key(format!(
                "{KEY_FILE_ENV} points the unseal key at {}, so a re-home would move \
                 whatever that file holds and not what the credential store holds; \
                 unset it and run this again",
                p.display()
            )));
        }
    }

    // Read through the same seam the server reads through, so a store that
    // only the legacy path can open is exactly the one this can move.
    let before = match native_key_read() {
        Ok(hex) => hex,
        Err(native) => legacy_security_read().ok_or(native)?,
    };
    // Validate before touching anything: a rehome that deletes a live item and
    // then discovers the value was never a key is the one outcome with no
    // recovery, and this costs nothing. Trimmed on the way through, so the
    // item this writes back is clean of the trailing newline the `security`
    // read carries — `decode_key_hex` tolerates it, the byte-for-byte
    // read-back comparison below would not have to.
    let _ = decode_key_hex(&before, "the credential store item")?;
    let before = Zeroizing::new(before.trim().to_string());

    let rescue = rescue_path()?;
    write_private(&rescue, before.as_bytes())?;

    // From here on, every error names the rescue file.
    let entry = keychain_entry()?;
    match entry.delete_credential() {
        Ok(()) => {}
        // Nothing native to delete: the item exists only where the legacy
        // read found it (or, on a host whose default keychain has been moved,
        // somewhere this API does not look — see the module header). Adding is
        // still the right next move.
        Err(keyring::Error::NoEntry) => {}
        Err(e) => {
            return Err(SecretsError::Key(format!(
                "{}; {}",
                keyring_error("delete", &e),
                rescue_hint(&rescue)
            )));
        }
    }
    entry.set_password(&before).map_err(|e| {
        SecretsError::Key(format!(
            "{}; {}",
            keyring_error("write", &e),
            rescue_hint(&rescue)
        ))
    })?;

    // Read back and *compare*, constant-time. A bare "the read succeeded"
    // would pass against a stale item in another keychain, or against any
    // item that happens to be readable under these coordinates.
    let after = native_key_read()
        .map_err(|e| SecretsError::Key(format!("{e}; {}", rescue_hint(&rescue))))?;
    if !Secret::new(before.to_string()).matches(&after) {
        return Err(SecretsError::Key(format!(
            "the re-homed `{KEYCHAIN_SERVICE}` item reads back as a different value than \
             was written — this host may have more than one keychain in play; {}",
            rescue_hint(&rescue)
        )));
    }

    // ...and that the store itself still opens under it, which is the thing
    // an operator actually cares about. No third credential-store read: the
    // bytes just read back are the ones under test.
    let key = decode_key_hex(&after, "the credential store item")
        .map_err(|e| SecretsError::Key(format!("{e}; {}", rescue_hint(&rescue))))?;
    let salt = b64_field(&path, "salt", &file.salt)?;
    decrypt_all(&derive_kek(&key, &salt), &path, &file)
        .map_err(|e| SecretsError::Key(format!("{e}; {}", rescue_hint(&rescue))))?;

    std::fs::remove_file(&rescue).or_else(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(SecretsError::Key(format!(
            "the unseal key was re-homed, but the rescue copy could not be removed ({e}); \
             delete {} by hand",
            rescue.display()
        ))),
    })?;

    Ok(format!(
        "unseal key re-homed into this host's credential store (service {KEYCHAIN_SERVICE}); \
         read back, and the sealed store opens under it"
    ))
}

// ---------------------------------------------------------------------------
// CLI operations (`tasks secrets …`)
// ---------------------------------------------------------------------------

/// Create the sealed store: generate salt and unseal key, park the key in the
/// Keychain (or `key_file` when given — mandatory off macOS), write the empty
/// store. Refuses to overwrite an existing store: re-running `init` over live
/// ciphertext would strand every entry behind a discarded key.
pub fn init(data_dir: &Path, key_file: Option<&Path>) -> Result<PathBuf, SecretsError> {
    let path = store_path(data_dir);
    if path.exists() {
        return Err(SecretsError::AlreadyInitialized(path));
    }

    let mut key = Zeroizing::new(vec![0u8; UNSEAL_KEY_BYTES]);
    rand::rngs::OsRng.fill_bytes(&mut key);
    let hex: Zeroizing<String> = Zeroizing::new(key.iter().map(|b| format!("{b:02x}")).collect());

    let key_source = match key_file {
        Some(kf) => {
            write_private(kf, hex.as_bytes())?;
            KeySource::File(kf.to_path_buf())
        }
        None if cfg!(target_os = "macos") => {
            keychain_write(&hex)?;
            KeySource::Keychain
        }
        None => {
            return Err(SecretsError::Key(
                "no Keychain on this platform — pass --key-file <path>".into(),
            ));
        }
    };

    let mut salt = [0u8; SALT_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let file = SealedFile {
        version: STORE_VERSION,
        key_source,
        salt: B64.encode(salt),
        entries: Default::default(),
    };
    write_store(&path, &file)?;
    Ok(path)
}

/// Seal `value` under `name`, replacing any previous entry. The running
/// server picks the write up off the file's mtime — no restart.
pub fn set(data_dir: &Path, name: SecretName, value: &str) -> Result<(), SecretsError> {
    let path = store_path(data_dir);
    let mut file = read_store(&path)?;
    let key = resolve_unseal_key(&file)?;
    let salt = b64_field(&path, "salt", &file.salt)?;
    let kek = derive_kek(&key, &salt);
    file.entries.insert(
        name.as_str().to_string(),
        encrypt_entry(&kek, name.as_str(), value),
    );
    write_store(&path, &file)
}

/// Remove `name` from the store. Returns whether it was present.
pub fn remove(data_dir: &Path, name: SecretName) -> Result<bool, SecretsError> {
    let path = store_path(data_dir);
    let mut file = read_store(&path)?;
    let removed = file.entries.remove(name.as_str()).is_some();
    if removed {
        write_store(&path, &file)?;
    }
    Ok(removed)
}

/// What the store holds — names and timestamps, never values. Requires no
/// unseal key: status must work exactly when the key is what's missing.
pub fn status(data_dir: &Path) -> Result<StoreStatus, SecretsError> {
    let path = store_path(data_dir);
    let file = read_store(&path)?;
    let mut entries: Vec<EntryStatus> = file
        .entries
        .iter()
        .filter_map(|(name, entry)| {
            SecretName::parse(name).map(|name| EntryStatus {
                name,
                set_at: entry.set_at,
            })
        })
        .collect();
    entries.sort_by_key(|e| e.name.as_str());
    Ok(StoreStatus {
        key_source: key_location(&file).to_string(),
        path,
        entries,
    })
}

fn read_store(path: &Path) -> Result<SealedFile, SecretsError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SecretsError::NotInitialized(path.to_path_buf()));
        }
        Err(e) => return Err(e.into()),
    };
    parse_file(path, &bytes)
}

/// Write `file` atomically (tempfile + rename) with 0600 permissions, its
/// parent created 0700. Atomic so the server's mtime-triggered reload can
/// never observe a half-written store.
fn write_store(path: &Path, file: &SealedFile) -> Result<(), SecretsError> {
    let json = serde_json::to_vec_pretty(file).expect("SealedFile serialization is infallible");
    write_private(path, &json)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SecretsError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Which of the three resolution steps answered for one secret — the *location*
/// of a value, never the value.
///
/// This is what a diagnostic prints. It is structurally incapable of leaking a
/// credential: there is no value in the type, so no rendering of it, however
/// careless, can put one on a terminal. [`Secrets::source_of`] is [`Secrets::get`]
/// with the value thrown away and this kept, resolving through the same order in
/// the same function, so the two can never disagree about who won — which is the
/// only property that makes the answer worth printing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CredentialSource {
    /// The sealed store under the data dir. What production should say.
    Sealed,
    /// The named environment variable, captured at boot.
    Environment(&'static str),
    /// The host's Claude Code `apiKeyHelper` — Anthropic only.
    ApiKeyHelper(PathBuf),
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sealed => f.write_str("the sealed store"),
            Self::Environment(var) => write!(f, "{var} in the environment"),
            Self::ApiKeyHelper(path) => write!(f, "apiKeyHelper {}", path.display()),
        }
    }
}

// ---------------------------------------------------------------------------
// The runtime handle
// ---------------------------------------------------------------------------

struct Cache {
    mtime: Option<SystemTime>,
    entries: HashMap<SecretName, Secret>,
}

/// One boot-captured fallback: the value, and where it was read from.
///
/// The location rides *with* the value rather than being recomputed on demand,
/// because recomputing it would re-run the `apiKeyHelper` — a subprocess, at
/// report time, whose answer could differ from the one actually in use.
struct EnvFallback {
    value: Secret,
    source: CredentialSource,
}

struct Inner {
    /// `None` for [`Secrets::for_tests`], which is env-map-only.
    path: Option<PathBuf>,
    /// Present once the store has been opened. Behind a lock because a store
    /// created *after* boot is unlocked lazily, once.
    kek: RwLock<Option<Key>>,
    late_unlock_attempted: AtomicBool,
    /// Boot-captured environment fallbacks, each with the location it came
    /// from. Read only when the sealed store has no entry for the name.
    env: HashMap<SecretName, EnvFallback>,
    cache: RwLock<Cache>,
}

/// The server's live view of the sealed store plus its environment fallbacks.
/// Cheap to clone; reads re-check the sealed file's mtime, so a
/// `tasks secrets set` against a running server takes effect on the next
/// read.
#[derive(Clone)]
pub struct Secrets(Arc<Inner>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names only — this rides inside `Config`, which derives Debug.
        f.debug_struct("Secrets")
            .field("sealed", &self.0.path)
            .field(
                "env_fallbacks",
                &self.0.env.keys().map(|n| n.as_str()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Secrets {
    /// Open the store under `data_dir` (hard error if it exists and cannot be
    /// opened) and capture the environment fallbacks. Called once, from
    /// `Config::from_env`.
    pub fn open(data_dir: &Path) -> Result<Self, SecretsError> {
        let path = store_path(data_dir);
        let mut kek = None;
        let mut cache = Cache {
            mtime: None,
            entries: HashMap::new(),
        };
        match std::fs::metadata(&path) {
            Ok(meta) => {
                let file = read_store(&path)?;
                let key = resolve_unseal_key(&file)?;
                let salt = b64_field(&path, "salt", &file.salt)?;
                let derived = derive_kek(&key, &salt);
                cache.entries = decrypt_all(&derived, &path, &file)?;
                cache.mtime = meta.modified().ok();
                kek = Some(derived);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let env = env_fallbacks();
        for name in SecretName::ALL {
            if env.contains_key(&name) && !cache.entries.contains_key(&name) {
                warn!(
                    secret = name.as_str(),
                    "raw {} in the environment — works, but `tasks secrets set {}` \
                     is where it should live",
                    name.env_var(),
                    name
                );
            }
        }

        Ok(Self(Arc::new(Inner {
            path: Some(path),
            kek: RwLock::new(kek),
            late_unlock_attempted: AtomicBool::new(false),
            env,
            cache: RwLock::new(cache),
        })))
    }

    /// A handle with no sealed store and exactly these values, for tests and
    /// for callers that already resolved their credentials.
    pub fn for_tests(github_token: Option<&str>, anthropic_api_key: Option<&str>) -> Self {
        let mut env = HashMap::new();
        for (name, value) in [
            (SecretName::GithubToken, github_token),
            (SecretName::AnthropicApiKey, anthropic_api_key),
        ] {
            if let Some(value) = value {
                env.insert(
                    name,
                    EnvFallback {
                        value: Secret::new(value),
                        source: CredentialSource::Environment(name.env_var()),
                    },
                );
            }
        }
        Self(Arc::new(Inner {
            path: None,
            kek: RwLock::new(None),
            late_unlock_attempted: AtomicBool::new(true),
            env,
            cache: RwLock::new(Cache {
                mtime: None,
                entries: HashMap::new(),
            }),
        }))
    }

    pub fn get(&self, name: SecretName) -> Option<Secret> {
        if self.0.path.is_some() {
            self.refresh_if_changed();
            let cache = self.0.cache.read().expect("secrets cache lock poisoned");
            if let Some(value) = cache.entries.get(&name) {
                return Some(value.clone());
            }
        }
        self.0.env.get(&name).map(|f| f.value.clone())
    }

    /// Where [`Self::get`] would find `name` — or `None` if it would find
    /// nothing.
    ///
    /// The same three steps in the same order, deliberately written as one
    /// `match` beside `get`'s body rather than as a second traversal
    /// somewhere else: a report that says "the sealed store answers for this"
    /// while the environment is what the server actually spends is worse than
    /// no report, and two hand-written versions of one resolution order is how
    /// that happens.
    pub(crate) fn source_of(&self, name: SecretName) -> Option<CredentialSource> {
        if self.0.path.is_some() {
            self.refresh_if_changed();
            let cache = self.0.cache.read().expect("secrets cache lock poisoned");
            if cache.entries.contains_key(&name) {
                return Some(CredentialSource::Sealed);
            }
        }
        self.0.env.get(&name).map(|f| f.source.clone())
    }

    /// A handle that resolves nothing and knows it — for a caller that could
    /// not open the store at all.
    ///
    /// Distinct from [`Self::for_tests`] with two `None`s only in what it
    /// says at the call site: doctor carries on past an unopenable store so
    /// that every non-credential check still runs, and the thing it must not
    /// do is then report "no credentials" as if it had looked.
    pub(crate) fn unresolvable() -> Self {
        Self::for_tests(None, None)
    }

    pub fn github_token(&self) -> Option<Secret> {
        self.get(SecretName::GithubToken)
    }

    pub fn anthropic_api_key(&self) -> Option<Secret> {
        self.get(SecretName::AnthropicApiKey)
    }

    pub fn github_configured(&self) -> bool {
        self.github_token().is_some()
    }

    /// Re-read the sealed file when its mtime moved. Errors keep the previous
    /// view (and the previous mtime, so the next read retries) — a rotation
    /// that fails to parse must not un-configure a running server.
    fn refresh_if_changed(&self) {
        let Some(path) = self.0.path.as_deref() else {
            return;
        };
        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        {
            let cache = self.0.cache.read().expect("secrets cache lock poisoned");
            if cache.mtime == mtime {
                return;
            }
        }
        // The store appeared after a boot that had none: unlock it now, once.
        // One credential-store read at most, ever — a failure is warned and
        // not retried until a restart. That bound mattered when this was a
        // subprocess (no `security` storm on the poll path) and it matters
        // more now: on macOS it is what stands between a locked or unfamiliar
        // keychain and one access dialog per poll interval.
        if mtime.is_some() && self.0.kek.read().expect("kek lock poisoned").is_none() {
            if self.0.late_unlock_attempted.swap(true, Ordering::SeqCst) {
                return;
            }
            match read_store(path).and_then(|file| {
                let key = resolve_unseal_key(&file)?;
                let salt = b64_field(path, "salt", &file.salt)?;
                Ok(derive_kek(&key, &salt))
            }) {
                Ok(derived) => {
                    *self.0.kek.write().expect("kek lock poisoned") = Some(derived);
                }
                Err(e) => {
                    warn!(error = %e, "a sealed store appeared but could not be unlocked; \
                           restart to pick it up");
                    return;
                }
            }
        }
        let mut cache = self.0.cache.write().expect("secrets cache lock poisoned");
        match mtime {
            None => {
                if !cache.entries.is_empty() {
                    warn!("sealed secret store removed; falling back to the environment");
                }
                cache.entries.clear();
                cache.mtime = None;
            }
            Some(new_mtime) => {
                let kek = self.0.kek.read().expect("kek lock poisoned");
                let Some(kek) = kek.as_ref() else {
                    return;
                };
                match read_store(path).and_then(|file| decrypt_all(kek, path, &file)) {
                    Ok(entries) => {
                        cache.entries = entries;
                        cache.mtime = Some(new_mtime);
                    }
                    Err(e) => {
                        warn!(error = %e, "sealed store changed but could not be re-read; \
                               keeping the previous values");
                    }
                }
            }
        }
    }
}

fn decrypt_all(
    kek: &Key,
    path: &Path,
    file: &SealedFile,
) -> Result<HashMap<SecretName, Secret>, SecretsError> {
    let mut out = HashMap::new();
    for (name, entry) in &file.entries {
        let Some(known) = SecretName::parse(name) else {
            warn!(
                name,
                "sealed store holds an entry this binary does not know; ignoring it"
            );
            continue;
        };
        out.insert(known, decrypt_entry(kek, path, name, entry)?);
    }
    Ok(out)
}

/// The environment fallbacks, captured once at boot: `GITHUB_TOKEN`,
/// `ANTHROPIC_API_KEY`, and — for the Anthropic key only — the host's Claude
/// Code `apiKeyHelper` script, exactly the chain `agent_credentials_env` used
/// to resolve before it was replaced by leases.
fn env_fallbacks() -> HashMap<SecretName, EnvFallback> {
    let mut out = HashMap::new();
    for name in SecretName::ALL {
        if let Ok(value) = std::env::var(name.env_var())
            && !value.is_empty()
        {
            out.insert(
                name,
                EnvFallback {
                    value: Secret::new(value),
                    source: CredentialSource::Environment(name.env_var()),
                },
            );
        }
    }
    if let std::collections::hash_map::Entry::Vacant(entry) = out.entry(SecretName::AnthropicApiKey)
        && let Some((key, helper)) = anthropic_key_from_host_helper()
    {
        entry.insert(EnvFallback {
            value: key,
            source: CredentialSource::ApiKeyHelper(helper),
        });
    }
    out
}

/// The output of the host's `~/.claude/anthropic_key.sh`, when one exists —
/// with the script's path, so a report can name *which* helper answered rather
/// than only that one did.
fn anthropic_key_from_host_helper() -> Option<(Secret, PathBuf)> {
    let helper = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".claude/anthropic_key.sh"))
        .filter(|p| p.exists())?;
    match Command::new(&helper).output() {
        Ok(out) if out.status.success() => {
            let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if key.is_empty() {
                None
            } else {
                tracing::info!("anthropic credential: host apiKeyHelper");
                Some((Secret::new(key), helper))
            }
        }
        Ok(out) => {
            warn!(status = %out.status, "apiKeyHelper failed");
            None
        }
        Err(e) => {
            warn!(error = %e, "could not run apiKeyHelper");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_with_key_file(dir: &TempDir) -> PathBuf {
        let key_file = dir.path().join("unseal.key");
        init(dir.path(), Some(&key_file)).expect("init");
        key_file
    }

    #[test]
    fn seal_and_read_back_through_the_handle() {
        let dir = TempDir::new().unwrap();
        init_with_key_file(&dir);
        set(dir.path(), SecretName::GithubToken, "ghp_sealed_value").unwrap();

        let secrets = Secrets::open(dir.path()).unwrap();
        assert_eq!(secrets.github_token().unwrap().expose(), "ghp_sealed_value");
        assert!(secrets.github_configured());
    }

    /// `source_of` is `get` with the value thrown away and the location kept,
    /// so the two must agree about who wins. Sealed outranks the environment
    /// in **both** — the failure this pins is a report that says "the sealed
    /// store answers for this" while the server spends the environment's copy,
    /// or the reverse.
    #[test]
    fn sealed_outranks_the_environment_in_get_and_in_source_of_alike() {
        let dir = TempDir::new().unwrap();
        init_with_key_file(&dir);
        set(dir.path(), SecretName::GithubToken, "sealed-value").unwrap();

        let mut secrets = Secrets::open(dir.path()).unwrap();
        // A boot-captured environment fallback, injected the way `open` would
        // have found one.
        let inner = Arc::get_mut(&mut secrets.0).expect("sole owner");
        inner.env.insert(
            SecretName::GithubToken,
            EnvFallback {
                value: Secret::new("environment-value"),
                source: CredentialSource::Environment("GITHUB_TOKEN"),
            },
        );

        assert_eq!(
            secrets.get(SecretName::GithubToken).unwrap().expose(),
            "sealed-value"
        );
        assert_eq!(
            secrets.source_of(SecretName::GithubToken),
            Some(CredentialSource::Sealed)
        );
    }

    /// It names a *location*, and there is no value in the type to leak.
    #[test]
    fn a_source_names_a_location_and_never_a_value() {
        let secrets = Secrets::for_tests(Some("ghp_do_not_print_me"), None);
        let source = secrets.source_of(SecretName::GithubToken).unwrap();
        assert_eq!(source, CredentialSource::Environment("GITHUB_TOKEN"));
        let rendered = format!("{source} {source:?}");
        assert!(rendered.contains("GITHUB_TOKEN"));
        assert!(!rendered.contains("ghp_do_not_print_me"));
        // ...and it agrees with `get` about which name resolves at all.
        assert_eq!(secrets.source_of(SecretName::AnthropicApiKey), None);
        assert!(secrets.get(SecretName::AnthropicApiKey).is_none());
    }

    /// The handle doctor carries on with after a store it could not open.
    /// "Nothing resolves" is what it says; the caller is what turns that into
    /// "we cannot tell".
    #[test]
    fn the_unresolvable_handle_answers_nothing() {
        let secrets = Secrets::unresolvable();
        for name in SecretName::ALL {
            assert!(secrets.get(name).is_none());
            assert!(secrets.source_of(name).is_none());
        }
        assert!(!secrets.github_configured());
    }

    #[test]
    fn a_set_after_open_is_visible_without_reopening() {
        let dir = TempDir::new().unwrap();
        init_with_key_file(&dir);
        set(dir.path(), SecretName::GithubToken, "before").unwrap();
        let secrets = Secrets::open(dir.path()).unwrap();
        assert_eq!(secrets.github_token().unwrap().expose(), "before");

        // mtime granularity on some filesystems is one second; write with a
        // nudged clock by rewriting until the mtime moves.
        let path = store_path(dir.path());
        let old = std::fs::metadata(&path).unwrap().modified().unwrap();
        set(dir.path(), SecretName::GithubToken, "after").unwrap();
        while std::fs::metadata(&path).unwrap().modified().unwrap() == old {
            std::thread::sleep(std::time::Duration::from_millis(20));
            set(dir.path(), SecretName::GithubToken, "after").unwrap();
        }
        assert_eq!(secrets.github_token().unwrap().expose(), "after");
    }

    #[test]
    fn the_wrong_unseal_key_refuses_to_open() {
        let dir = TempDir::new().unwrap();
        let key_file = init_with_key_file(&dir);
        set(dir.path(), SecretName::GithubToken, "value").unwrap();
        std::fs::write(&key_file, "0".repeat(64)).unwrap();
        let err = Secrets::open(dir.path()).unwrap_err();
        assert!(matches!(err, SecretsError::Decrypt { .. }), "{err}");
    }

    #[test]
    fn ciphertext_is_bound_to_its_entry_name() {
        let dir = TempDir::new().unwrap();
        init_with_key_file(&dir);
        set(dir.path(), SecretName::GithubToken, "value").unwrap();
        // Move the ciphertext under the other name: same KEK, wrong AAD.
        let path = store_path(dir.path());
        let mut file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let entry = file["entries"]["github-token"].take();
        file["entries"] = serde_json::json!({ "anthropic-api-key": entry });
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let err = Secrets::open(dir.path()).unwrap_err();
        assert!(matches!(err, SecretsError::Decrypt { .. }), "{err}");
    }

    #[test]
    fn init_refuses_to_run_twice() {
        let dir = TempDir::new().unwrap();
        init_with_key_file(&dir);
        let err = init(dir.path(), Some(&dir.path().join("other.key"))).unwrap_err();
        assert!(matches!(err, SecretsError::AlreadyInitialized(_)), "{err}");
    }

    #[test]
    fn secret_string_debug_is_redacted() {
        let s = Secret::new("ghp_very_secret");
        assert_eq!(format!("{s:?}"), "<redacted>");
        // The sentinel has to be a string no *name* could contain: `Secrets`
        // legitimately prints `github-token`, so a value of "tok" fails this
        // assertion while the redaction it tests is working perfectly.
        let handle = Secrets::for_tests(Some("ghp_very_secret"), None);
        let rendered = format!("{handle:?}");
        assert!(!rendered.contains("ghp_very_secret"), "{rendered}");
        // ...and the assertion is falsifiable: the name it *does* carry is
        // there, so this is not passing because the output is empty.
        assert!(rendered.contains("github-token"), "{rendered}");
    }

    /// `status` is the one command whose job is to answer "where does the
    /// unseal key come from?", and the override exists for the case where the
    /// recorded source is what is failing. Reporting the header there sends an
    /// operator to debug a Keychain the process is not going to read.
    #[test]
    fn status_reports_the_key_source_that_will_actually_be_used() {
        let keychain = SealedFile {
            version: 1,
            key_source: KeySource::Keychain,
            salt: String::new(),
            entries: Default::default(),
        };
        // No override: the header is the answer.
        assert!(
            key_location_with(&keychain, None)
                .to_string()
                .contains("Keychain"),
        );
        // An override outranks it, and says so.
        let overridden = key_location_with(&keychain, Some("/tmp/unseal.key".into())).to_string();
        assert!(overridden.contains("/tmp/unseal.key"), "{overridden}");
        assert!(overridden.contains(KEY_FILE_ENV), "{overridden}");
        assert!(!overridden.contains("Keychain"), "{overridden}");
        // An empty value is not an override — the same reading
        // `resolve_unseal_key` makes, which is the point of sharing this.
        assert!(
            key_location_with(&keychain, Some("  ".into()))
                .to_string()
                .contains("Keychain"),
        );
    }

    /// Every way a credential store can refuse has to end in something an
    /// operator can do. Asserted off the pure mapper, because no host `make
    /// test` runs on has a credential store to produce these for real.
    #[test]
    fn every_credential_store_failure_names_the_way_out() {
        let cases = [
            keyring::Error::NoEntry,
            keyring::Error::NoDefaultStore,
            keyring::Error::PlatformFailure("the keychain is locked".into()),
            keyring::Error::NotSupportedByStore("no".into()),
        ];
        for err in cases {
            let mapped = keyring_error("read", &err).to_string();
            assert!(
                mapped.contains(KEY_FILE_ENV),
                "{err:?} maps to a message with no escape hatch: {mapped}"
            );
            assert!(
                mapped.contains("tasks secrets init"),
                "{err:?} maps to a message that does not say how to create the item: {mapped}"
            );
        }
        // ...and the assertion is falsifiable: the two arms differ, so this is
        // not passing because one sentence covers everything.
        let missing = keyring_error("read", &keyring::Error::NoEntry).to_string();
        assert!(missing.contains("has no"), "{missing}");
        assert!(
            !missing.contains("could not read"),
            "a missing item is not a failed read: {missing}"
        );
    }

    /// A file-keyed store has no credential-store item to move, and a rehome
    /// that got that far would delete whatever else answers to these
    /// coordinates. The refusal is also the cheap half to get right — so the
    /// test proves the store is untouched on the way out, which is the part
    /// that would actually cost something.
    #[test]
    fn rehome_refuses_a_file_keyed_store() {
        let dir = TempDir::new().unwrap();
        init_with_key_file(&dir);
        set(dir.path(), SecretName::GithubToken, "value").unwrap();

        let err = rehome_key(dir.path()).unwrap_err();
        let rendered = err.to_string();
        assert!(matches!(err, SecretsError::Key(_)), "{rendered}");
        assert!(
            rendered.contains("not a credential-store item"),
            "{rendered}"
        );

        // The refusal did no damage on its way out.
        let secrets = Secrets::open(dir.path()).unwrap();
        assert_eq!(secrets.github_token().unwrap().expose(), "value");
    }

    #[test]
    fn secret_names_round_trip() {
        for name in SecretName::ALL {
            assert_eq!(SecretName::parse(name.as_str()), Some(name));
        }
        assert_eq!(SecretName::parse("nope"), None);
    }
}
