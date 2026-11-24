---
name: dwave
description: Find and inspect Drips Wave issues and track watched issues using the unofficial dwave CLI.
---

# Drips Wave issues

Use this skill when asked to discover or triage Wave issues, or to check a local watchlist. `dwave` is an unofficial, read-only client; it cannot apply or comment for the user.

Check that `dwave` is installed and consult `dwave --help` or a subcommand's `--help` if needed. Use `--json` for commands whose output you need to interpret. Report errors and incomplete or stale results rather than treating them as complete.

Prefer cached results where practical. Narrow searches before increasing request budgets, and do not repeatedly force fresh requests. Watch checks are on demand, not notifications.

Wave data is a snapshot. A missing Wave PR link does not establish that no PR exists; use GitHub cross-checking when available, and present the result with its freshness and any limitations. Verify availability before recommending that someone apply.

## Recipes

```sh
dwave --json preset list
dwave --json issues --preset untouched --limit 20
dwave --json issues --repo owner/repo --pending 0 --no-pr --explain
dwave --json issue show 'owner/repo#123' --github
dwave --json watch add 'owner/repo#123'
dwave --json watch list
dwave --json watch check --github
dwave --json watch log 'owner/repo#123'
```

Replace the example issue with the user's actual reference. `untouched` means never applied and no Wave PR link; zero pending applications alone is a broader condition. Use `--offline` for cached results and `--fresh` only when a refresh is needed. Inspect the `dwave/v1` envelope's `meta` fields, including `as_of`, `partial`, and warnings. For `watch check` and `issues --new-since-last`, exit 10 means confirmed changes (possibly with partial results), 0 means a complete check with none, 1 means an error or a partial check without confirmed changes, and 2 means a usage error. Always inspect partial status and warnings. Summarize the observed changes without claiming that an application or other write was made.

The first successful watch check for an issue records the baseline. Use `watch check` without `--github` to check only Wave, and `watch rm REF` when asked to stop tracking an issue.

Resolving an uncached `owner/repo#N` or GitHub issue URL may make a public GitHub title lookup even without `--github`; matching against Wave uses the exact repository and issue number. `--offline` suppresses the lookup. GitHub timeline PR signals may include references that do not close the issue, and visibility depends on token permissions.
