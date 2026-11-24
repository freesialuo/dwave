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
dwave --json watch check
dwave --json watch log
```

Replace the example issue with the user's actual reference. `untouched` means never applied and no Wave PR link; zero pending applications alone is a broader condition. Use `--offline` for cached results and `--fresh` only when a refresh is needed. Inspect the `dwave/v1` envelope's `meta` fields, including `as_of`, `partial`, and warnings. Treat exit code 10 from `watch check` as changes found, 0 as no changes, 1 as an error, and 2 as a usage error. Summarize the observed changes without claiming that an application or other write was made.
