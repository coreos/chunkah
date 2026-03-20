---
name: release-notes
description: Use when reshaping a raw GitHub changelog draft into polished release notes with user-impact-focused summaries.
---

# Release Notes

Reshape a raw GitHub-generated changelog into polished release notes.

## Input format

The input file contains entries in this raw GitHub changelog format:

```text
* <PR title> by @<author> in <PR URL>
```

## Process

1. Use `gh release list --repo <owner>/<repo> --limit 5` to fetch recent
   release tags, then `gh release view <tag> --repo <owner>/<repo>` to
   read their release notes. Use these as a reference for how items were
   categorized in past releases.
2. For each PR entry, parse the PR number from its URL, then use
   `gh pr view <number> --json body,commits` to
   fetch the PR description and commit messages. Fetch all PR details in
   parallel to minimize latency.
3. From the PR body and commit messages, determine the user-facing impact
   of the change (e.g. performance improvement, bug fix, new feature).
4. Write a brief summary emphasizing impact to users, not implementation
   details. Keep it to 1-3 sentences.
5. Categorize each item into one of the sections below, using the
   previous release notes as a guide for consistency.

## Output format

Group items under these section headings (omit sections with no items):

- **Major changes** - New features, significant behavior changes,
  performance improvements, or breaking changes that users should know
  about.
- **Minor changes** - Small enhancements, new options, improved
  warnings, or documentation updates that are user-visible.
- **Internal changes** - Refactoring, CI fixes, test improvements, or
  code cleanup with no direct user-facing impact.
- **Packaging changes** - Dependency bumps, spec file changes, Packit
  fixes, or build system updates.

Rewrite each entry as:

```text
* <user-impact summary> (<PR URL>)
```

## Example

Input:

```text
* tar: stream file reads instead of buffering into `Vec<u8>`
  by @jlebon in https://github.com/coreos/chunkah/pull/72
```

Output:

<!-- markdownlint-disable MD013 -->

```markdown
## Major changes

* Dramatically reduced memory usage by streaming file reads
  instead of buffering entire files and tar layers into memory,
  cutting peak RSS to ~75 MB (from ~6.5 GB of allocations for
  an FCOS image) (https://github.com/coreos/chunkah/pull/72)
```

<!-- markdownlint-enable MD013 -->

## Final step

After writing the output, check for spelling and grammar issues and fix
them.

Write the final result to `release-notes.md` in the repo root.
