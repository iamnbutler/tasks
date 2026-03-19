# Specification Contribution Guide

This document defines the conventions for writing and maintaining specification documents in this
directory.

## Document Types

### Main Specification (`spec.md`)

The platform specification covering the overall architecture, domain model, operating modes,
and high-level behavior. This is the primary reference for understanding the system.

### Detail Specifications

Companion documents that expand on specific subsystems in implementation-focused detail. These
are for readers implementing or deeply understanding a particular component.

Current detail specs:
- `github.md` — GitHub integration (polling, GraphQL, normalized model)
- `session-runtime.md` — Container provider, supervisor protocol, workspace provisioning

## Document Format

### Required Header

Every specification document must begin with:

```markdown
# [Title]

Status: Draft | Review | Stable

[One-paragraph purpose statement explaining what this document covers and its relationship
to the main spec.]
```

**Status values:**
- **Draft** — Work in progress, may change significantly
- **Review** — Ready for review, expected to stabilize
- **Stable** — Finalized, changes require versioning

### Section Numbering

Use numbered sections for easy cross-referencing:

```markdown
## 1. First Section

### 1.1 Subsection

### 1.2 Another Subsection

## 2. Second Section
```

### Table of Contents

Documents over 200 lines should include a table of contents after the header.

## Cross-Referencing

### Within the Same Document

Reference sections by number: `(see §3.2)` or `as described in §1`.

### Between Documents

Reference other specs with filename and section: `(see github.md §2.1)` or
`(spec.md §10 Sessions)`.

### Back-References

Detail specs should note which main spec sections they expand in their overview:

```markdown
This document specifies [...]. It is a companion to the main spec (spec.md §10 Sessions
and Agent Runner, §11 Workspace Management).
```

## Code References

When spec sections are referenced in code, use the format:

```rust
// See spec/session-runtime.md §4.1
```

This allows searching the codebase for spec references and identifying drift.

## Keeping Specs in Sync

### Adding New Sections

1. Add the section to the appropriate spec document
2. Update cross-references in other specs if needed
3. Update SUMMARY.md if adding a new document
4. Search for existing references to ensure consistency

### Removing or Moving Sections

1. Search for references: `rg "spec-name.md §N"`
2. Update all references before removing/moving
3. Consider leaving a note: `[Section moved to other-spec.md §M]`

### Code-Spec Alignment

For protocol definitions and API schemas, keep the spec and code in sync:
- Protocol types should match between spec and `crates/runtime/src/protocol/`
- Update both when making changes

## File Organization

```
spec/
├── README.md          # Overview and document index
├── SUMMARY.md         # mdBook-style table of contents
├── CONTRIBUTING.md    # This file
├── CHANGELOG.md       # Version history
├── VERSION            # Current version number
├── spec.md            # Main platform specification
├── github.md          # GitHub integration detail spec
└── session-runtime.md # Session runtime detail spec
```
