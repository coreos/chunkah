---
description: Polish raw GitHub release notes into user-impact-focused summaries
---

# Polish release notes

Polish raw GitHub-generated release notes for chunkah.

## Input

The first argument (`$1`) is the version to release (e.g. `v0.4.0` or
`0.4.0`). Strip any `v` prefix to get the bare version (e.g. `0.4.0`).

If a second argument (`$2`) is provided, it is the path to a file
containing the raw release notes. Read the raw notes from that file.

If no second argument is provided, run
`just release generate-notes <bare-version>` to fetch raw notes from
GitHub. Request to run in the host context if you get a 403.

The raw notes contain entries in this format:

```text
* <PR title> by @<author> in <PR URL>
```

## Process

1. Use `gh release list --repo coreos/chunkah --limit 5` to fetch
   recent release tags, then
   `gh release view <tag> --repo coreos/chunkah` to read their release
   notes. Use these as a reference for how items were categorized in
   past releases.
2. For each PR entry, parse the PR number from its URL, then use
   `gh pr view <number> --repo coreos/chunkah --json body,commits` to
   fetch the PR description and commit messages. Fetch all PR details
   in parallel to minimize latency.
3. From the PR body and commit messages, determine the user-facing
   impact of the change (e.g. performance improvement, bug fix, new
   feature).
4. Write a brief summary emphasizing impact to users, not
   implementation details. Keep it to 1-3 sentences.
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

If the raw notes contain a "## New Contributors" section, preserve it
verbatim at the bottom of the output, after all other sections.

## Example

Input:

```text
* tar: stream file reads instead of buffering into `Vec<u8>`
  by @jlebon in https://github.com/coreos/chunkah/pull/72
```

Output:

```markdown
## Major changes

* Dramatically reduced memory usage by streaming file reads
  instead of buffering entire files and tar layers into memory,
  cutting peak RSS to ~75 MB (from ~6.5 GB of allocations for
  an FCOS image) (https://github.com/coreos/chunkah/pull/72)
```

## Final steps

After drafting the output, check for spelling and grammar issues and
fix them. Write the final result to `.release-notes-<bare-version>.md`
in the repo root (e.g. `.release-notes-0.4.0.md`).
