//! The app icon is four pieces that each fail *silently* (#986).
//!
//! macOS does not warn about a `CFBundleIconFile` that resolves to nothing —
//! it draws the generic blank, which is the exact state this issue is about,
//! returning with nothing to diagnose it. And nothing on any CI machine runs
//! the recipe that would notice: `app-install` is `check-darwin`-gated, and
//! `app-gpui` is not a workspace member, so `make test` never runs the app's
//! own tests.
//!
//! So these live in the server's tree, for the reason
//! `crates/tasks/tests/disclaimer.rs` states in its own header: a guard
//! nothing runs is not a guard. `make test` runs these; `make app-test` is
//! what runs the one unit test in `app-gpui/src/about.rs`.
//!
//! What is asserted is **structure, never the picture**. The mark is generated
//! from constants at the top of `app-gpui/icon/appicon.py`, and a redesign must
//! not have to come here: the icon stays free to change and only stops being
//! free to stop being an icon.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // `crates/tasks` -> the workspace root, so the tests do not depend on
    // which directory the runner happened to start in.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/tasks has two ancestors")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn the_plist_names_the_icon_without_its_extension() {
    let plist = read("app-gpui/Info.plist.in");

    let key = plist
        .find("<key>CFBundleIconFile</key>")
        .expect("Info.plist.in declares no CFBundleIconFile; the bundle draws the generic blank");
    let value = plist[key..]
        .lines()
        .nth(1)
        .expect("CFBundleIconFile has no value")
        .trim();
    // macOS appends `.icns` itself, so `AppIcon.icns` here resolves to
    // `AppIcon.icns.icns` — which draws the blank, silently.
    assert_eq!(
        value, "<string>AppIcon</string>",
        "CFBundleIconFile takes a base name, not a filename"
    );

    // That key names an entry in a compiled asset catalog (Assets.car). This
    // bundle has none, so it would point at nothing. The *declaration* is what
    // must be absent — the template's comment names the key to say why it is
    // not there, and a guard that forbade the word would forbid the
    // explanation along with the mistake.
    assert!(
        !plist.contains("<key>CFBundleIconName</key>"),
        "CFBundleIconName needs an asset catalog and this bundle has none"
    );
}

#[test]
fn app_install_copies_the_icon_into_resources() {
    let makefile = read("Makefile");
    let recipe = makefile
        .split_once("\napp-install: check-darwin\n")
        .expect("no app-install target")
        .1;
    // The recipe is one shell line continued with backslashes; it ends at the
    // first line that is neither indented nor blank.
    let body: String = recipe
        .lines()
        .take_while(|l| l.starts_with('\t') || l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    // The plist key and this copy are two halves of one change: either alone
    // leaves the app drawing the blank.
    assert!(
        body.contains("app-gpui/AppIcon.icns"),
        "app-install does not copy the icon out of app-gpui/"
    );
    assert!(
        body.contains("Contents/Resources/AppIcon.icns"),
        "app-install does not put the icon where CFBundleIconFile looks for it"
    );

    // One line covers every bundle only because the release chain routes
    // through this same target rather than keeping a second copy of its own.
    assert!(
        makefile.contains("app-install APP_BUNDLE=$(RELEASE_BUNDLE)"),
        "release-bundle no longer routes through app-install; the signed \
         download can now differ from the bundle that was tested"
    );
}

/// Exactly what `iconutil -c icns` emits for a full `.iconset`, in its order.
///
/// 32, 256 and 512 each appear **twice** under two OSTypes — a @1x slot and a
/// @2x slot of a smaller nominal size — and a reader selects by type, so
/// dropping a "duplicate" costs the icon one of the two slots.
const MEMBERS: &[(&[u8; 4], u32)] = &[
    (b"icp4", 16),
    (b"ic11", 32),
    (b"icp5", 32),
    (b"ic12", 64),
    (b"ic07", 128),
    (b"ic13", 256),
    (b"ic08", 256),
    (b"ic14", 512),
    (b"ic09", 512),
    (b"ic10", 1024),
];

#[test]
fn the_icns_is_a_complete_member_set() {
    let blob = fs::read(repo_root().join("app-gpui/AppIcon.icns")).expect("read AppIcon.icns");

    assert_eq!(&blob[..4], b"icns", "bad magic");
    let declared = u32::from_be_bytes(blob[4..8].try_into().unwrap()) as usize;
    // The single most likely way to get a file that exists and shows nothing:
    // a length field written without its own 8-byte header. It parses far
    // enough to look right and then runs off the end.
    assert_eq!(
        declared,
        blob.len(),
        "header length disagrees with the file"
    );

    let mut at = 8;
    let mut seen: Vec<([u8; 4], u32)> = Vec::new();
    while at < blob.len() {
        let tag: [u8; 4] = blob[at..at + 4].try_into().unwrap();
        let n = u32::from_be_bytes(blob[at + 4..at + 8].try_into().unwrap()) as usize;
        assert!(n >= 8 && at + n <= blob.len(), "chunk {tag:?} overruns");
        let payload = &blob[at + 8..at + n];
        assert_eq!(
            &payload[..8],
            b"\x89PNG\r\n\x1a\n",
            "chunk {tag:?} is not a PNG"
        );
        let w = u32::from_be_bytes(payload[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(payload[20..24].try_into().unwrap());
        assert_eq!(w, h, "chunk {tag:?} is not square");
        seen.push((tag, w));
        at += n;
    }

    let want: Vec<([u8; 4], u32)> = MEMBERS.iter().map(|(t, s)| (**t, *s)).collect();
    assert_eq!(seen, want, "the member set drifted");
}

#[test]
fn the_mark_has_a_source_and_the_about_window_reads_it() {
    // The icon is generated, not hand-placed binary: it has a source that can
    // be reviewed, diffed and regenerated on a machine with no Mac in reach.
    let generator = read("app-gpui/icon/appicon.py");
    assert!(
        generator.contains("AppIcon.icns"),
        "the generator no longer writes the icns"
    );
    assert!(
        generator.contains("--check"),
        "the generator lost --check, the only way to prove a committed \
         artifact matches its source"
    );

    for mark in ["app-gpui/icon/AppIcon.svg", "app-gpui/icon/AppIconMark.svg"] {
        let svg = read(mark);
        assert!(svg.starts_with("<svg "), "{mark} has no svg root");
        assert!(svg.trim_end().ends_with("</svg>"), "{mark} is truncated");
    }

    // The About window and the Dock are two renderings of one set of numbers.
    // It reads the tight variant: `AppIcon.svg` carries the 100px margin every
    // macOS icon needs and inline that is dead weight.
    let about = read("app-gpui/src/about.rs");
    assert!(
        about.contains("include_bytes!(\"../icon/AppIconMark.svg\")"),
        "the About window no longer embeds the mark"
    );
}
