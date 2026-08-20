//! Guards for `site/` — the landing page published at `nate.rip/tasks/`
//! (#995) — and for the claim that publishing it does not disturb.
//!
//! Three assertions, and they protect three different things.
//!
//! The first two are the load-bearing ones, and they are the *inverse* of the
//! single assertion that stood here before #1015. Seven doc sites and a
//! CLAUDE.md bullet used to rest on GitHub being *structurally incapable* of
//! objecting to a change that does not work — first as "this repository has no
//! `.github/workflows`" (one `ls` away from checkable), then as "no workflow
//! here produces a pull-request check" (a property of every `on:` block). CI
//! is now here, so that premise is gone and its opposite is what has to hold:
//! **the suite runs on every commit pushed to this repository**, which is what
//! lets those same sites say `clean` now carries evidence. The way that claim
//! goes quietly false is no longer a `pull_request` trigger appearing — it is a
//! `branches:` or `paths:` filter appearing on CI's push trigger, after which
//! a Builder branch reads `clean` because nothing ever ran on it. That is the
//! one way `clean` can lie, so it is checked here.
//!
//! The second is narrower and is about secrets rather than evidence:
//! `pull_request_target` runs fork code with this repository's own token, and
//! nothing here has any use for it.
//!
//! The second is that the disclaimer on the published page still says what
//! the README says. `site/check.sh` makes the same comparison, and the
//! duplication is deliberate rather than an oversight: that script runs in
//! the Pages workflow, before a *deploy*, and this test runs in `make test`,
//! before a *merge*. This pipeline merges its own pull requests, so a drift
//! only the script catches is one that has already landed on `main` and whose
//! first symptom is a page that stopped publishing. Do not delete either as
//! redundant.

use std::fs;
use std::path::{Path, PathBuf};

/// Every doc site that rests on CI running against every commit here. Named in
/// the failure message so whoever trips this a year from now can find them
/// without grepping for a sentence that no longer says what they searched.
const DOC_SITES: &[&str] = &[
    "crates/tasks/src/github.rs (the `Landing` enum doc)",
    "crates/tasks/src/github.rs (the doc on `clear_says_what_it_does_not_mean`)",
    "crates/tasks/src/brief.rs (the doc on `verification_line`)",
    "crates/tasks/src/orchestrator.rs (the doc on `landing_section`)",
    "CLAUDE.md (the landing bullet: \"An open PR is chased like every other stage\")",
    "site/README.md (\"Deploying\" — why pages.yml is not the workflow that matters)",
    ".github/workflows/ci.yml (the header comment)",
];

/// The workflow that has to keep running on everything.
const CI_WORKFLOW: &str = ".github/workflows/ci.yml";

fn repo_root() -> PathBuf {
    // `crates/tasks` -> the repository root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

/// Drop `#` comments, so a workflow that *explains* why it has no
/// `pull_request` trigger does not read as having one. `pages.yml` carries
/// exactly that comment, deliberately — the next person to touch it will be
/// tempted, and the reason belongs beside the temptation.
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        let mut quote: Option<char> = None;
        let mut end = line.len();
        for (at, ch) in line.char_indices() {
            match quote {
                Some(open) if ch == open => quote = None,
                Some(_) => {}
                None if ch == '\'' || ch == '"' => quote = Some(ch),
                // A `#` starts a comment only at the start of a token.
                None if ch == '#' && (at == 0 || line[..at].ends_with([' ', '\t'])) => {
                    end = at;
                    break;
                }
                None => {}
            }
        }
        out.push_str(&line[..end]);
        out.push('\n');
    }
    out
}

/// The workflow's `on:` block: the rest of the `on:` line plus every
/// following line indented under it, up to the next top-level key.
///
/// Returns `None` when the file has no top-level `on:` at all, which is a
/// workflow that can never run and is not this test's business.
fn triggers(source: &str) -> Option<String> {
    let source = strip_comments(source);
    let mut lines = source.lines();
    let mut block = String::new();

    loop {
        let line = lines.next()?;
        // GitHub writes `on:`; YAML 1.1 also lets it be quoted, and some
        // parsers render the unquoted form as the boolean `true`.
        for key in ["on:", "\"on\":", "'on':", "true:"] {
            if let Some(rest) = line.strip_prefix(key) {
                block.push_str(rest);
                block.push('\n');
                for line in lines.by_ref() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    // A line at column 0 ends the block.
                    if !line.starts_with([' ', '\t']) {
                        return Some(block);
                    }
                    block.push_str(line);
                    block.push('\n');
                }
                return Some(block);
            }
        }
    }
}

fn workflow_files() -> Vec<PathBuf> {
    let dir = repo_root().join(".github").join("workflows");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yml" || extension == "yaml")
        })
        .collect();
    files.sort();
    files
}

fn relative(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The premise under every autonomous merge in this pipeline, since #1015
/// inverted it: **the suite runs against every commit pushed here.**
///
/// Before CI, the sites in [`DOC_SITES`] said GitHub could not object to a
/// change that does not work, and the way that went false was a `pull_request`
/// trigger appearing. Now they say GitHub's verdict on the head commit is
/// evidence a merge may cite, and the way *that* goes false is quieter: a
/// `branches:` or `paths:` filter on CI's push trigger, after which a Builder
/// branch reads `clean` because nothing ever ran on it. Nothing goes red. The
/// pull request looks better, not worse.
///
/// So this asserts the trigger's *shape*, not merely its presence: `push`,
/// with no filter under it.
#[test]
fn ci_runs_the_suite_on_every_push() {
    let path = repo_root().join(CI_WORKFLOW);
    let source = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "reading {path}: {error}\n\n\
             {CI_WORKFLOW} is what makes GitHub's verdict on a commit worth citing, and \
             these sites say it exists:\n\n  {sites}\n\n\
             If CI is genuinely going away, rewrite `Landing`'s reading of `mergeable_state` \
             in the same commit that deletes it. Do not delete this test.",
            path = path.display(),
            sites = DOC_SITES.join("\n  "),
        )
    });
    let block = triggers(&source).unwrap_or_else(|| {
        panic!("{CI_WORKFLOW} has no top-level `on:` block, so it can never run")
    });
    assert!(
        block.contains("push"),
        "{CI_WORKFLOW} does not trigger on `push`.\n\nThe `on:` block read was:\n{block}"
    );
    for filter in ["branches:", "branches-ignore:", "paths:", "paths-ignore:"] {
        assert!(
            !block.contains(filter),
            "{CI_WORKFLOW}'s triggers carry a `{filter}` filter.\n\n\
             A filtered push trigger means some commits are never checked, and this pipeline \
             merges its own pull requests citing GitHub's verdict on the head commit — so an \
             unchecked branch reads `clean` because nothing ran, which is the one way `clean` \
             can lie. It fails upward: the pull request looks better, and nothing goes red.\n\n\
             These sites say every commit here is checked:\n\n  {sites}\n\n\
             The `on:` block read was:\n{block}",
            sites = DOC_SITES.join("\n  "),
        );
    }
}

/// `pull_request_target` runs the *base* branch's workflow with this
/// repository's own secrets, against code from a fork. Nothing here needs it,
/// and this repository's token can push branches and merge pull requests.
///
/// Unrelated to the evidence question above — kept as its own assertion so a
/// change to one cannot quietly relax the other.
#[test]
fn no_workflow_runs_fork_code_with_our_secrets() {
    for path in workflow_files() {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
        let Some(block) = triggers(&source) else {
            continue;
        };
        assert!(
            !block.contains("pull_request_target"),
            "{} has an `on: pull_request_target` trigger, which runs fork code with this \
             repository's secrets — including a token that can push branches and merge \
             pull requests.\n\nThe `on:` block read was:\n{block}",
            relative(&path),
        );
    }
}

// --- the disclaimer -----------------------------------------------------------

/// Text between `<!-- disclaimer:start -->` and `<!-- disclaimer:end -->`,
/// exclusive. `None` when the markers are not both present.
fn between_markers(source: &str) -> Option<&str> {
    const START: &str = "<!-- disclaimer:start -->";
    const END: &str = "<!-- disclaimer:end -->";
    let start = source.find(START)? + START.len();
    let end = source[start..].find(END)? + start;
    Some(&source[start..end])
}

/// Both copies are prose wrapped to fit their own file, one as HTML and one
/// as Markdown, so both are reduced to a bare sequence of words before they
/// are compared: a reflow of either must pass, and a changed word must fail.
///
/// Kept in step with `normalise` in `site/check.sh` — see the module doc for
/// why there are two of these.
fn normalise(source: &str) -> String {
    let mut text = String::with_capacity(source.len());

    // Fenced-code delimiters, which exist in the Markdown copy and not in the
    // HTML one.
    for line in source.lines() {
        if line.trim_start().starts_with("```") {
            text.push('\n');
            continue;
        }
        text.push_str(line);
        text.push('\n');
    }

    // HTML comments, then HTML tags. Tags are removed rather than replaced
    // with a space: the Markdown writes `(`images/scout/Dockerfile`,` with no
    // space around the code span, and the HTML wraps the same words in
    // `<code>` with none either.
    let text = strip_delimited(&text, "<!--", "-->");
    let text = strip_delimited(&text, "<", ">");

    let text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");

    // Per line: a leading list bullet, heading hashes or a block quote. The
    // bullet must be a marker *followed by a space* — `-H` on the continuation
    // line of the curl command is not a list item.
    let mut stripped = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim_start();
        let rest = ["- ", "* ", "+ ", "> "]
            .iter()
            .find_map(|marker| trimmed.strip_prefix(marker))
            .or_else(|| {
                let hashes = trimmed.trim_start_matches('#');
                (hashes.len() != trimmed.len()).then(|| hashes.trim_start())
            })
            .unwrap_or(trimmed);
        stripped.push_str(rest);
        stripped.push('\n');
    }

    stripped
        .replace("**", "")
        .replace('`', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_delimited(source: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find(close) {
            Some(end) => &rest[start + end + close.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// The page's risk block is the README's words, and the README owns them.
#[test]
fn disclaimer_on_the_page_matches_the_readme() {
    let readme = read("README.md");
    let page = read("site/index.html");

    let readme_block = between_markers(&readme).expect(
        "README.md has no <!-- disclaimer:start --> / <!-- disclaimer:end --> markers.\n\n\
         The README's `## Read this first` (#984) is the canonical risk copy and the \
         landing page publishes a copy of it. The markers are what lets both this test and \
         `site/check.sh` find it. Put them back around that section without changing the \
         words between them.",
    );
    let page_block = between_markers(&page).expect(
        "site/index.html has no <!-- disclaimer:start --> / <!-- disclaimer:end --> \
         markers.\n\n\
         The landing page must carry the README's risk copy — it is the second thing on \
         the page, before the architecture, because a warning below the thing it warns \
         about is one you read after deciding. Copy the README's words; do not write new \
         ones.",
    );

    assert_eq!(
        normalise(readme_block),
        normalise(page_block),
        "\n\nThe risk disclaimer in README.md and the one in site/index.html have drifted.\n\n\
         They are the same words in two files by contract, and the README is canonical \
         (#984 owns the wording). Whichever one you changed, make the other match. Both \
         copies are compared with whitespace collapsed and markup stripped, so re-wrapping \
         either file is free and only a changed *word* fails.\n\n\
         This is caught here rather than only in `site/check.sh` because that script runs \
         at deploy time: by then the drift is on `main` and the symptom is a landing page \
         that has silently stopped publishing.\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two shapes the same words take in the two files really do compare
    /// equal — and a changed word really does not. Without this, a normaliser
    /// bug that flattened both sides to nothing would leave the guard above
    /// passing on anything.
    #[test]
    fn normalise_survives_a_reflow_and_not_a_rewrite() {
        let markdown = "## Read this first\n\n\
             - **The server can.** It pushes branches, opens\n  pull requests.\n\n  \
             ```sh\n  curl -X POST localhost:4800/charter/land_builds \\\n       -d 'x'\n  ```\n";
        let html = "<h2>Read this first</h2>\n<ul>\n<li><strong>The server can.</strong> \
             It pushes branches, opens pull requests.\n\
             <pre><code>curl -X POST localhost:4800/charter/land_builds \\\n     -d 'x'\
             </code></pre></li>\n</ul>\n";

        assert_eq!(normalise(markdown), normalise(html));
        assert!(normalise(markdown).contains("Read this first The server can. It pushes branches"));
        // The continuation line of a shell command is not a list bullet.
        assert!(normalise(markdown).contains("\\ -d 'x'"));
        assert_ne!(
            normalise(markdown),
            normalise(&html.replace("pushes", "may push"))
        );
    }

    #[test]
    fn a_trigger_block_is_read_and_a_comment_about_one_is_not() {
        let yaml = "name: x\n\
             # never on: pull_request — it would create a check\n\
             on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n\
             jobs:\n  build:\n    steps: []\n";
        let block = triggers(yaml).expect("a top-level `on:`");
        assert!(block.contains("push"));
        assert!(block.contains("workflow_dispatch"));
        assert!(
            !block.contains("pull_request"),
            "a comment mentioning the trigger must not read as the trigger: {block}"
        );
        assert!(
            !block.contains("jobs"),
            "the block stops at the next top-level key"
        );

        assert!(
            triggers("on: [push, pull_request]\njobs: {}\n")
                .expect("inline list")
                .contains("pull_request")
        );
        assert!(
            triggers("on:\n  pull_request:\n    types: [opened]\n")
                .expect("mapping")
                .contains("pull_request")
        );
    }
}
