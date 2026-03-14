---
# Shared reporting fragment - imported by other workflows
---

## Report Formatting

Follow the content structure and formatting guidelines from the imported formatting fragment above.

## Reporting Workflow Run Information

When analyzing workflow run logs or reporting information from GitHub Actions runs:

### 1. Workflow Run ID Formatting

**Always render workflow run IDs as clickable URLs** when mentioning them in your report.

**Format:**

`````markdown
[§12345](https://github.com/owner/repo/actions/runs/12345)
`````

### 2. Document References for Workflow Runs

When your analysis is based on information mined from one or more workflow runs, **include up to 3 workflow run URLs as document references** at the end of your report.

**Format:**

`````markdown
---

**References:**
- [§12345](https://github.com/owner/repo/actions/runs/12345)
`````

**Guidelines:**

- Include **maximum 3 references** to keep reports concise
- Choose the most relevant or representative runs
- Always use the actual URL from the workflow run data
