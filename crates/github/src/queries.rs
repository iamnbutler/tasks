//! GraphQL query strings for the GitHub API — spec github.md §3.

/// Fragment shared by issue list and single-issue queries.
const ISSUE_FIELDS: &str = r#"
    number
    id
    title
    body
    state
    stateReason
    author { login ... on User { id } ... on Bot { id } ... on Mannequin { id } }
    labels(first: 100) {
        nodes { name color }
    }
    assignees(first: 100) {
        nodes { login id }
    }
    milestone {
        title
        number
        state
    }
    comments(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
            id
            author { login ... on User { id } ... on Bot { id } ... on Mannequin { id } }
            body
            createdAt
            updatedAt
        }
    }
    parent {
        number
        title
        state
        id
    }
    subIssues(first: 50) {
        nodes {
            number
            title
            state
            id
        }
    }
    timelineItems(first: 100, itemTypes: [CROSS_REFERENCED_EVENT, MARKED_AS_BLOCKED_BY_EVENT, UNMARKED_AS_BLOCKED_BY_EVENT]) {
        nodes {
            ... on CrossReferencedEvent {
                source {
                    ... on PullRequest {
                        number
                        title
                        state
                        id
                    }
                }
            }
            ... on MarkedAsBlockedByEvent {
                blockingIssue {
                    number
                    title
                    state
                    id
                }
            }
            ... on UnmarkedAsBlockedByEvent {
                blockingIssue {
                    number
                    title
                    state
                    id
                }
            }
        }
    }
    createdAt
    updatedAt
    closedAt
"#;

/// Fragment shared by PR list and single-PR queries.
const PR_FIELDS: &str = r#"
    number
    id
    title
    body
    state
    headRefName
    headRefOid
    baseRefName
    isDraft
    mergeable
    author { login ... on User { id } ... on Bot { id } ... on Mannequin { id } }
    labels(first: 100) {
        nodes { name color }
    }
    assignees(first: 100) {
        nodes { login id }
    }
    reviewDecision
    reviews(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
            id
            author { login ... on User { id } ... on Bot { id } ... on Mannequin { id } }
            state
            body
            submittedAt
        }
    }
    comments(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
            id
            author { login ... on User { id } ... on Bot { id } ... on Mannequin { id } }
            body
            createdAt
            updatedAt
        }
    }
    closingIssuesReferences(first: 50) {
        nodes {
            number
            title
            state
            id
        }
    }
    createdAt
    updatedAt
    closedAt
    mergedAt
"#;

/// List issues for a repository with filtering and pagination (spec github.md §3.1).
pub fn list_issues_query() -> String {
    format!(
        r#"query ListIssues($owner: String!, $name: String!, $first: Int!, $after: String, $states: [IssueState!], $labels: [String!], $since: DateTime) {{
  repository(owner: $owner, name: $name) {{
    issues(
      first: $first
      after: $after
      states: $states
      labels: $labels
      filterBy: {{ since: $since }}
      orderBy: {{ field: UPDATED_AT, direction: DESC }}
    ) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{
        {ISSUE_FIELDS}
      }}
    }}
  }}
}}"#
    )
}

/// Fetch a single issue by number (spec github.md §3.3).
pub fn get_issue_query() -> String {
    format!(
        r#"query GetIssue($owner: String!, $name: String!, $number: Int!) {{
  repository(owner: $owner, name: $name) {{
    issue(number: $number) {{
      {ISSUE_FIELDS}
    }}
  }}
}}"#
    )
}

/// Fetch additional comment pages for an issue.
pub fn issue_comments_query() -> &'static str {
    r#"query IssueComments($owner: String!, $name: String!, $number: Int!, $first: Int!, $after: String!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      comments(first: $first, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
            id
            author { login ... on User { id } ... on Bot { id } ... on Mannequin { id } }
            body
            createdAt
            updatedAt
        }
      }
    }
  }
}"#
}

/// List pull requests for a repository with filtering and pagination (spec github.md §3.2).
pub fn list_pull_requests_query() -> String {
    format!(
        r#"query ListPullRequests($owner: String!, $name: String!, $first: Int!, $after: String, $states: [PullRequestState!]) {{
  repository(owner: $owner, name: $name) {{
    pullRequests(
      first: $first
      after: $after
      states: $states
      orderBy: {{ field: UPDATED_AT, direction: DESC }}
    ) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{
        {PR_FIELDS}
      }}
    }}
  }}
}}"#
    )
}

/// Fetch a single pull request by number (spec github.md §3.3).
pub fn get_pull_request_query() -> String {
    format!(
        r#"query GetPullRequest($owner: String!, $name: String!, $number: Int!) {{
  repository(owner: $owner, name: $name) {{
    pullRequest(number: $number) {{
      {PR_FIELDS}
    }}
  }}
}}"#
    )
}

/// Fetch additional comment pages for a pull request.
pub fn pr_comments_query() -> &'static str {
    r#"query PullRequestComments($owner: String!, $name: String!, $number: Int!, $first: Int!, $after: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      comments(first: $first, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
            id
            author { login ... on User { id } ... on Bot { id } ... on Mannequin { id } }
            body
            createdAt
            updatedAt
        }
      }
    }
  }
}"#
}

/// Fetch additional review pages for a pull request.
pub fn pr_reviews_query() -> &'static str {
    r#"query PullRequestReviews($owner: String!, $name: String!, $number: Int!, $first: Int!, $after: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviews(first: $first, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
            id
            author { login ... on User { id } ... on Bot { id } ... on Mannequin { id } }
            state
            body
            submittedAt
        }
      }
    }
  }
}"#
}
