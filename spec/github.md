# GitHub Integration

Status: Draft

This document specifies the GitHub integration layer — how the platform discovers, fetches, and
normalizes work from GitHub. It is a companion to the main spec (spec.md §11 Issue Tracker
Integration, §3.2 Scheduler).

## 1. Overview

The GitHub crate is the platform's read interface to GitHub. It fetches issues, pull requests, and
their associated metadata from GitHub's GraphQL API, normalizes them into a stable internal model,
and provides a polling mechanism for discovering new and changed work.

The crate does not write to GitHub. All GitHub mutations (comments, labels, state changes, PR
creation) are performed by agents working inside their sessions, using the `gh` CLI with
credentials injected into the container environment (session-runtime.md §3.1).

```
Server
  └── Scheduler (spec.md §3.2)
        │
        ├── uses GitHubClient to poll for changes
        ├── normalizes responses into internal model
        ├── emits events to the event bus
        │
        └── GitHubClient (this crate)
              ├── GraphQL queries against api.github.com
              ├── rate limit tracking
              └── pagination handling
```

The crate is consumed by the scheduler but is otherwise independent — it has no dependency on the
event system, server, or runtime crates.

## 2. Normalized Model

The crate normalizes GitHub's API responses into a stable internal model. The rest of the system
works with these types, never with raw GitHub API shapes.

### 2.1 Issue

- `owner` (string) — repository owner
- `repo` (string) — repository name
- `number` (u64) — issue number
- `node_id` (string) — GitHub's global GraphQL node ID (used for pagination and cross-references)
- `title` (string)
- `body` (string or null)
- `state` (enum) — Open, Closed
- `state_reason` (enum or null) — Completed, NotPlanned, Reopened (GitHub's close reason)
- `labels` (list of Label)
- `assignees` (list of User)
- `milestone` (Milestone or null)
- `comments` (list of Comment) — full comment history, ordered chronologically
- `parent` (ParentIssueRef or null) — parent issue if this is a sub-issue
- `sub_issues` (list of SubIssueRef) — issues linked as sub-issues via GitHub's sub-issue feature
- `blocked_by` (list of BlockingIssueRef) — issues that block this one (must be resolved before
  this issue can be worked on)
- `linked_pull_requests` (list of LinkedPR) — PRs that reference this issue (via closing keywords
  or manual links)
- `author` (User)
- `created_at` (timestamp)
- `updated_at` (timestamp)
- `closed_at` (timestamp or null)

### 2.2 Pull Request

- `owner` (string) — repository owner
- `repo` (string) — repository name
- `number` (u64) — PR number
- `node_id` (string)
- `title` (string)
- `body` (string or null)
- `state` (enum) — Open, Closed, Merged
- `head_ref` (string) — source branch name
- `head_sha` (string) — current head commit SHA
- `base_ref` (string) — target branch name
- `is_draft` (bool)
- `mergeable` (enum or null) — Mergeable, Conflicting, Unknown (GitHub may not have computed this
  yet)
- `labels` (list of Label)
- `assignees` (list of User)
- `review_decision` (enum or null) — Approved, ChangesRequested, ReviewRequired
- `reviews` (list of Review) — all reviews, ordered chronologically
- `comments` (list of Comment) — issue-level comments (not review comments)
- `linked_issues` (list of LinkedIssueRef) — issues this PR closes/references
- `author` (User)
- `created_at` (timestamp)
- `updated_at` (timestamp)
- `closed_at` (timestamp or null)
- `merged_at` (timestamp or null)

### 2.3 Supporting Types

**Label:** `name` (string), `color` (string)

**User:** `login` (string), `node_id` (string)

**Milestone:** `title` (string), `number` (u64), `state` (Open | Closed)

**Comment:** `id` (string), `author` (User), `body` (string), `created_at` (timestamp),
`updated_at` (timestamp)

**Review:** `id` (string), `author` (User), `state` (Approved | ChangesRequested | Commented |
Dismissed), `body` (string or null), `submitted_at` (timestamp)

**ParentIssueRef:** `number` (u64), `title` (string), `state` (Open | Closed), `node_id` (string)

**SubIssueRef:** `number` (u64), `title` (string), `state` (Open | Closed), `node_id` (string)

**BlockingIssueRef:** `number` (u64), `title` (string), `state` (Open | Closed), `node_id` (string)

**LinkedPR:** `number` (u64), `title` (string), `state` (Open | Closed | Merged),
`node_id` (string)

**LinkedIssueRef:** `number` (u64), `title` (string), `state` (Open | Closed),
`node_id` (string)

## 3. GraphQL Queries

All data is fetched via GitHub's GraphQL API (`POST https://api.github.com/graphql`). The crate
provides three query categories.

### 3.1 Repository Issues

Fetches issues for a repository, with filtering and pagination.

**Parameters:**
- `owner`, `repo` — repository identifier
- `states` (optional) — filter by Open, Closed, or both. Default: Open only
- `labels` (optional) — filter to issues with any of these labels
- `since` (optional) — only issues updated after this timestamp (for polling)
- `first` / `after` (optional) — cursor-based pagination

**Returns:** Paginated list of Issue (§2.1), including all nested fields (comments, labels,
assignees, sub-issues, linked PRs) in a single query.

### 3.2 Repository Pull Requests

Fetches PRs for a repository, with filtering and pagination.

**Parameters:**
- `owner`, `repo` — repository identifier
- `states` (optional) — filter by Open, Closed, Merged. Default: Open only
- `since` (optional) — only PRs updated after this timestamp
- `first` / `after` (optional) — cursor-based pagination

**Returns:** Paginated list of PullRequest (§2.2), including reviews, comments, and linked issues
in a single query.

### 3.3 Single Item Fetch

Fetches a single issue or PR by number, with full detail.

**Parameters:**
- `owner`, `repo`, `number`

**Returns:** Issue or PullRequest with all fields populated.

This is used when the scheduler needs to refresh a specific item (e.g., after an event indicates it
changed, or when fetching a linked issue referenced by another item).

### 3.4 Pagination

GitHub GraphQL uses cursor-based pagination. The crate handles this internally:

- Each query requests up to 100 items per page (GitHub's maximum).
- Comments and reviews are paginated within each item — the crate fetches all pages for these
  nested connections automatically.
- The client exposes a stream/iterator interface so callers don't manage cursors directly.
- A configurable maximum page limit prevents runaway queries on repositories with thousands of
  issues (default: 10 pages = 1000 items).

### 3.5 Rate Limiting

GitHub's GraphQL API has a point-based rate limit (typically 5,000 points per hour). Each query
costs a variable number of points depending on the fields and pagination depth requested.

The client tracks rate limit state from response headers:
- `x-ratelimit-remaining` — points remaining
- `x-ratelimit-reset` — when the budget resets

Behavior:
- If remaining points drop below a configurable threshold (default: 200), the client pauses
  requests and waits until the reset window.
- Rate limit state is exposed to callers so the scheduler can adjust its polling cadence.
- If a request receives a 403 with rate limit exceeded, the client waits for the reset time and
  retries once.

## 4. Client API

The `GitHubClient` is the public interface to the crate. It is a thin async wrapper around the
GraphQL queries, rate limit tracking, and response normalization.

### 4.1 Construction

```
GitHubClient::new(token: String) -> GitHubClient
```

Takes a personal access token. The token is sent as `Authorization: Bearer {token}` on all
requests. The client holds a single `reqwest::Client` internally for connection pooling.

An optional builder allows overriding:
- `base_url` — for GitHub Enterprise or testing against a mock server (default:
  `https://api.github.com`)
- `max_pages` — pagination limit (default: 10)
- `rate_limit_floor` — minimum remaining points before pausing (default: 200)

### 4.2 Methods

**Issues:**
- `list_issues(owner, repo, filters) -> Result<Vec<Issue>>` — paginated, returns all pages up to
  limit
- `get_issue(owner, repo, number) -> Result<Issue>` — single issue with full detail

**Pull Requests:**
- `list_pull_requests(owner, repo, filters) -> Result<Vec<PullRequest>>` — paginated
- `get_pull_request(owner, repo, number) -> Result<PullRequest>` — single PR with full detail

**Rate Limit:**
- `rate_limit() -> RateLimit` — current rate limit state (remaining points, reset time)

### 4.3 Filters

```
IssueFilters {
    states: Option<Vec<IssueState>>,
    labels: Option<Vec<String>>,
    since: Option<DateTime<Utc>>,
}

PullRequestFilters {
    states: Option<Vec<PullRequestState>>,
    since: Option<DateTime<Utc>>,
}
```

### 4.4 Errors

```
GitHubError {
    Auth            — 401, bad or expired token
    NotFound        — issue/PR/repo doesn't exist
    RateLimited     — rate limit exceeded after retry
    GraphQL(Vec)    — GitHub returned GraphQL-level errors
    Network         — connection/timeout failures
    Decode          — response didn't match expected shape
}
```

## 5. Polling and Discovery

The crate provides a higher-level polling interface on top of the raw client. This is what the
scheduler (spec.md §3.2) uses to discover new and changed work.

### 5.1 Repository Poller

The `RepoPoller` tracks the last-seen `updated_at` timestamp per repository and fetches only items
that changed since the last poll.

```
RepoPoller::new(client: GitHubClient, owner: String, repo: String) -> RepoPoller
```

**Methods:**
- `poll() -> Result<PollResult>` — fetches issues and PRs updated since the last successful poll.
  On first call, fetches all open items.
- `poll_issues() -> Result<Vec<Issue>>` — issues only
- `poll_pull_requests() -> Result<Vec<PullRequest>>` — PRs only

**PollResult:**
- `issues` — list of new or updated issues
- `pull_requests` — list of new or updated PRs
- `timestamp` — the `updated_at` high-water mark from this poll (used as `since` on the next call)
- `rate_limit` — rate limit state after this poll

### 5.2 Change Detection

The poller returns all items updated since the last poll. It is the caller's (scheduler's)
responsibility to determine what changed — the poller does not diff against previous state.

This is intentional. The scheduler already maintains task state and is the right place to compare
incoming GitHub state against internal state. The poller is a data-fetching layer, not a state
machine.

### 5.3 High-Water Mark

The poller tracks a single `since` timestamp per repository:

- After a successful poll, `since` advances to the maximum `updated_at` across all returned items.
- If a poll fails, `since` is not advanced — the next poll retries the same window.
- The timestamp is held in memory. If the server restarts, the first poll after restart fetches all
  open items (equivalent to a cold start). Persisting the high-water mark is a future optimization.

### 5.4 Polling Cadence

The poller does not own its own timer. The scheduler calls `poll()` on whatever cadence it chooses
(spec.md §3.2 says configurable). This keeps the crate free of tokio::time dependencies and
scheduling opinions.

## 6. Testing

### 6.1 Unit Tests

**Normalization tests.** Given raw GraphQL JSON responses (captured from real API calls or
hand-written), verify that normalization produces the correct model structs. These tests exercise
the deserialization and mapping logic without making network calls. Cover:

- Issues with all fields populated
- Issues with null/missing optional fields (no milestone, no assignees, closed without reason)
- PRs in each state (open, closed, merged) with varying mergeable/review states
- Nested pagination (issue with >100 comments)
- Sub-issues and linked PRs/issues
- Malformed or unexpected fields (should produce `Decode` errors, not panics)

**Rate limit tracking tests.** Verify that rate limit state is correctly parsed from response
headers and that the floor threshold triggers waiting behavior.

**Pagination tests.** Verify cursor handling across multiple pages, including the stop condition
when `has_next_page` is false or the page limit is reached.

**Filter construction tests.** Verify that `IssueFilters` and `PullRequestFilters` produce the
correct GraphQL query variables.

### 6.2 Integration Tests

Integration tests run against a real GitHub API. They are gated behind a feature flag
(`--features integration`) and require a `GITHUB_TOKEN` environment variable.

**Target repository:** Tests run against a public fixture repository (e.g., `tasks-test/fixture`)
with known issues, PRs, comments, and labels. The fixture repo is set up once and not modified by
tests — all operations are reads.

Tests:
- Fetch a known issue by number and verify all fields
- Fetch a known PR by number and verify all fields (including reviews)
- List open issues with label filter
- List open PRs
- Pagination across multiple pages (fixture repo needs enough issues)
- `since` filter returns only recently updated items
- Rate limit state is populated after a request
- Bad token returns `Auth` error
- Nonexistent repo returns `NotFound` error

### 6.3 Mock Server Tests

For testing polling behavior and error handling without depending on GitHub uptime or rate limits,
the crate includes tests that run a local HTTP server (using `wiremock` or similar) serving canned
GraphQL responses.

Tests:
- `RepoPoller` advances high-water mark after successful poll
- `RepoPoller` does not advance after failed poll
- Rate limit floor triggers wait behavior
- 403 rate-limit response triggers retry-after-reset
- Network timeout produces `Network` error
- GraphQL error response produces `GraphQL` error

## 7. Open Questions

- **Webhook support.** The spec (§11.4) mentions optional webhook push notifications. This crate
  covers the polling path. Webhook ingestion may be a separate module in the server crate, since it
  requires an HTTP endpoint and ties into the server's request handling.
- **GraphQL schema changes.** GitHub evolves its GraphQL schema. Sub-issues in particular are
  relatively new. The normalization layer should degrade gracefully if a field is absent from the
  response.
- **Nested pagination limits.** An issue with thousands of comments would require many nested
  pagination calls. A practical limit on nested page depth (e.g., 10 pages = 1000 comments) may
  be needed.
