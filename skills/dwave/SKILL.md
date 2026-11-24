---
name: dwave
description: Find and inspect Drips Wave issues and track watched issues using the unofficial dwave CLI.
---

# Drips Wave issues

Use this skill when asked to discover or triage Drips Wave issues, or to check a local watchlist. `dwave` is an unofficial, read-only client; it cannot apply or comment for the user.

Check that `dwave` is installed (`dwave --version`). If missing, install with `npm install -g @freesialuo/dwave` (Node.js 22+) or `cargo install --path crates/dwave` from `https://github.com/freesialuo/dwave`. Consult `dwave --help` or `dwave <command> --help` if needed. Use `--json` for commands whose output you interpret, and report errors or incomplete/stale results rather than treating them as complete.

Prefer cached results where practical. Use `--explain` before expensive queries, narrow searches with `--repo` or presets before raising `--max-requests`, and do not loop `--fresh`. Watch checks are on demand, not push notifications.

Wave data is a snapshot. A missing Wave PR link does not establish that no PR exists; use `--github` cross-checking when available and present the result with `meta.as_of`, `meta.partial`, and any warnings. Verify availability on Wave and GitHub before recommending that someone apply.

## Recipes

```sh
dwave --json waves --current
dwave --json preset list
dwave --json issues --preset untouched --limit 20
dwave --json issues --repo owner/repo --pending 0 --no-pr --explain
dwave --json sync --scope untouched
dwave --json issue show 'owner/repo#123' --github
dwave --json watch add 'owner/repo#123'
dwave --json watch list
dwave --json watch check --github
dwave --json watch log 'owner/repo#123'
```

Replace example references with actual values. `untouched` requires never having had an application and no Wave PR link; `--pending 0` is broader and can include issues with earlier rejected or withdrawn applications. Use `--offline` for cached reads and `--fresh` only when a refresh is needed.

For `watch check` and `issues --new-since-last`, exit `10` means confirmed changes (even if `meta.partial` is `true`), `0` means a complete check with no changes, `1` means an error or a partial check without confirmed changes, and `2` means a usage error. Always inspect `meta.partial` and `meta.warnings`. Summarize observed changes without claiming an application or other write was made.

The first successful `watch check` for an issue records its baseline; `issues --new-since-last` seeds its baseline on the first complete run. Use `watch check` without `--github` to check only Wave, and `watch rm REF` when asked to stop tracking an issue.

Resolving an uncached `owner/repo#N` or GitHub issue URL may perform a public GitHub title lookup even without `--github`; matching against Wave still requires an exact repository and issue number match. `--offline` suppresses that lookup. GitHub timeline PR signals may include references that do not close the issue, and visibility depends on token permissions.
