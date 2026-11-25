# dwave

An unofficial, read-only command-line tool and Rust library for discovering, caching, and tracking issues in [Drips Wave](https://www.drips.network) programs.

> [!NOTE]
> `dwave` is not affiliated with or endorsed by Drips. The Wave HTTP API (`https://wave-api.drips.network`) is undocumented and may change without notice. `dwave` never applies to issues or posts comments.

`dwave` combines server-filtered and local issue queries, a persistent SQLite cache with client-side rate limiting and request budgets, optional GitHub cross-checking, and an on-demand watchlist with snapshot history and script-friendly exit codes.

## Installation

### From npm (recommended)

Install globally with Node.js 22 or newer:

```sh
npm install -g @freesialuo/dwave
dwave --help
```

Or run without installing globally:

```sh
npx @freesialuo/dwave --help
```

The `@freesialuo/dwave` package bundles prebuilt native binaries for:

- `darwin-arm64` (macOS Apple Silicon)
- `darwin-x64` (macOS Intel)
- `linux-arm64` (Linux glibc, Ubuntu 22.04 baseline)
- `linux-x64` (Linux glibc, Ubuntu 22.04 baseline)
- `win32-x64` (Windows x64)

### From source (Rust)

Clone the repository and build with Cargo:

```sh
git clone https://github.com/freesialuo/dwave.git
cd dwave
cargo install --path crates/dwave
dwave --help
```

Or run directly from a checkout without installing:

```sh
cargo run --release -p dwave -- --help
```

## Quick Start

```sh
# Select a Wave program and view its active wave.
dwave program use stellar
dwave waves --current

# Browse a cheap server-filtered preset, or inspect a custom query plan first.
dwave issues --preset untouched --limit 20
dwave issues --repo owner/repo --pending 0 --no-pr --explain

# Inspect a specific issue with GitHub cross-checking, then watch it.
dwave issue show 'owner/repo#123' --github
dwave watch add 'owner/repo#123' --note 'Good candidate'
dwave watch list
dwave watch check --github --json
```

Replace `owner/repo` and `123` with a real repository and issue number. Issue references (`<ref>`) accept a Wave UUID, `OWNER/REPO#N`, or a GitHub issue URL (`https://github.com/OWNER/REPO/issues/N`).

## CLI Reference

Run `dwave <command> --help` for full option details on any subcommand.

### Commands

| Command | Description |
|---|---|
| `dwave program show [<slug>]` | Show details for `<slug>` or the active program |
| `dwave program use <slug>` | Persist `<slug>` as `default_program` |
| `dwave waves [--current]` | List waves in the selected program (`--current` filters to active waves) |
| `dwave issues [filters] [options]` | Query issues (after an implicit refresh when stale) |
| `dwave sync [--scope <preset>] [filters] [--full] [--explain]` | Populate or refresh the local cache for a scope |
| `dwave issue show <ref> [--github]` | Inspect one issue (with optional GitHub state, assignees, and timeline PRs) |
| `dwave watch add <ref>... [--note <text>]` | Add one or more issues to the local watchlist |
| `dwave watch rm <ref>...` | Remove one or more issues from the watchlist |
| `dwave watch list` | List watched issues and their notes |
| `dwave watch check [--github]` | Refresh watched issues and report changes since the last snapshot |
| `dwave watch log [<ref>] [--since <dur>]` | Show snapshot and change history (default `--since 7d`) |
| `dwave preset list` | List built-in and saved filter presets with estimated costs |
| `dwave preset save <name> [filters]` | Save a custom filter preset in `config.toml` |
| `dwave preset rm <name>` | Delete a custom preset or hide a built-in preset |
| `dwave cache stats` | Show row counts across the local SQLite cache |
| `dwave cache prune [--older-than <dur>]` | Prune cached entries older than `<dur>` (default `30d`) |
| `dwave config get <key>` | Read a configuration setting |
| `dwave config set <key> <value>` | Validate and persist a configuration setting |

### Global Options

| Option | Env / Notes |
|---|---|
| `--program <slug>` | Override `default_program` for one invocation (positional `<slug>` on `program show` takes precedence) |
| `--json` | Emit a single `dwave/v1` JSON envelope (conflicts with `--ndjson`) |
| `--ndjson` | Stream one `dwave/v1` JSON envelope per record |
| `--db <path>` | Override the SQLite cache path (`DWAVE_DB` env var, else platform data dir) |
| `--config <path>` | Override the TOML config path (`DWAVE_CONFIG` env var, else platform config dir) |
| `-v`, `--verbose` | Print request, retry, `304` unchanged, and byte metrics to `stderr` |
| `--no-color` | Disable ANSI color in human output (also respects `NO_COLOR`) |
| `--fresh` | Bypass TTLs and refresh requested data (conflicts with `--offline`) |
| `--offline` | Serve strictly from the local cache without network requests |
| `--max-requests <n>` | Override the per-invocation HTTP request cap (`>= 1`, includes retries and GitHub calls) |

### Filtering, Presets, and Query Modes

`dwave issues` and `dwave sync` support `--preset <name>` / `--scope <preset>` plus explicit filter flags. Explicit flags override preset fields; repeating `--repo` or `--label` replaces the preset's corresponding list. `issues` also accepts `--sort updated|points|created` (default `updated`), `--limit <n>` (default `20`), `--page <n>` (default `1`), `--new-since-last`, `--github`, `--full`, and `--explain`.

**Server-pushed vs. local filters:**

- **Server-pushed** (evaluated remotely and re-checked locally): `--state <state|all>` (default `open`), `--never-applied` / `--has-applications`, `--has-pr` / `--no-pr`, `--search <text>`, plus a single `--repo <OWNER/REPO>` once its Wave repository UUID is known in cache.
- **Local-only** (require syncing the matching remote scope first): `--pending <N|N..=M|..=M|N..>`, `--assigned` / `--unassigned`, `--min-points <n>`, `--max-points <n>`, repeatable `--label <name>` (AND), multiple `--repo <OWNER/REPO>` (OR), `--updated-since <dur>`, and `--sort points|created`.

**Execution modes (inspect with `--explain` without making requests; the plan lists the `remote` query and the `local` filters applied to cached rows):**

- **Passthrough**: When every active filter and sort is server-pushable (and `--full` / `--new-since-last` are not set), `dwave` fetches only enough pages to satisfy `--limit` and `--page`.
- **Scoped sync**: When any predicate or sort requires local evaluation, `dwave` syncs the narrowest remote scope into SQLite. Subsequent runs within `listing_ttl` make zero requests; afterwards they run incremental `updatedAt` syncs with an overlap window (`overlap`) and an automatic full resync every `full_interval` (24h) when the estimated page count fits the remaining budget. If `--max-requests` is hit mid-sync, `dwave` saves the cursor so the next run resumes and marks `meta.partial = true`. Overlapping runs in separate processes take an advisory scope lock and fall back to cached results with a warning rather than fetching twice.

**Built-in presets (`dwave preset list`):**

| Preset | Filter | Cost | Notes |
|---|---|---|---|
| `untouched` | `state=open`, `--never-applied`, `--no-pr` | `cheap` | Narrower than `--pending 0`: excludes issues with earlier withdrawn or rejected applications |
| `open-pending-0` | `state=open`, `--pending 0`, `--unassigned`, `--no-pr` | `broad` | Includes issues with past non-pending applications; combine with `--repo` or raise `--max-requests` |
| `unassigned` | `state=open`, `--unassigned`, `--no-pr` | `broad` | Open unassigned issues with no Wave PR link |
| `high-value` | `state=open`, `--unassigned`, `--min-points <high_value_min_points>` | `broad` | Default threshold is `200`, the official High tier (configurable via `dwave config set high_value_min_points <n>`) |
| `repo` | `state=open` | `repository-dependent` | Requires at least one `--repo OWNER/REPO` |

When saving a preset with `dwave preset save`, omitted `--state` defaults to `open`; pass `--state all` to match any state.

## JSON Output, Watch Events, and Automation

### `dwave/v1` Envelope

Passing `--json` outputs a single JSON document; `--ndjson` emits one envelope per record for array views:

```json
{
  "schema": "dwave/v1",
  "meta": {
    "program": "stellar",
    "as_of": "2026-11-24T21:00:00Z",
    "partial": false,
    "warnings": [],
    "requests": 2,
    "retries": 0,
    "not_modified": 0,
    "bytes": 18432
  },
  "data": [],
  "page": {
    "page": 1,
    "limit": 20,
    "total": 42
  }
}
```

Always inspect `meta.partial`, `meta.as_of`, and `meta.warnings` before treating results as complete.

### Watch Events

`watch check` refreshes each watched issue via the single-issue endpoint (respecting `issue_ttl`), optionally enriches via GitHub (`--github`), and diffs against the previous snapshot:

- `applications_changed` (`from`, `to`)
- `assigned` (`who`) / `unassigned`
- `assignees_changed` (`from`, `to`)
- `pr_linked` (`url`) / `pr_unlinked` (`url`)
- `state_changed` (`from`, `to`)
- `points_changed` (`from`, `to`)
- `github_changed` (`from`, `to`)
- `disappeared` / `reappeared`

The first successful `watch check` for an issue seeds its baseline snapshot without emitting changes. Similarly, `issues --new-since-last` seeds its baseline on the first complete run (returning no items) and delivers newly seen matches on subsequent runs, retaining any matches not yet delivered across pages.

### Exit Codes & Scheduled Jobs

| Exit Code | Meaning |
|---|---|
| `0` | Success (for `watch check` and `issues --new-since-last`: complete check with no changes) |
| `10` | Confirmed changes found (`watch check` or `issues --new-since-last`, even when `meta.partial` is `true`) |
| `1` | Runtime error, or a partial `watch check` / `--new-since-last` run with no confirmed changes |
| `2` | CLI usage or validation error |

Watch checks run on demand; invoke `dwave` from `cron`, `launchd`, or CI and handle exit code `10` explicitly (the `if` condition below is safe under `set -e`):

```sh
if dwave --json watch check --github > watch-changes.json; then
  echo "No changes"
else
  status=$?
  case "$status" in
    10) echo "Changes found; inspect watch-changes.json" ;;
    *) echo "Watch check failed ($status)" >&2; exit "$status" ;;
  esac
fi
```

Replace `watch check --github` with `issues --preset untouched --new-since-last` for scheduled discovery.

## Configuration & GitHub Access

`dwave` stores configuration in TOML (`config.toml` in the platform config directory, e.g. `~/Library/Application Support/dwave/config.toml` on macOS or `~/.config/dwave/config.toml` on Linux) and cache data in SQLite (`dwave.db` in the platform data directory).

### Configuration Keys

Durations accept `<n>s`, `<n>m`, `<n>h`, `<n>d`, or `<n>w`.

| Key | Default | Meaning |
|---|---|---|
| `default_program` | `"stellar"` | Default Wave program slug |
| `program_ttl` | `"7d"` | Cache TTL for program metadata |
| `waves_ttl` | `"6h"` | Cache TTL for wave listings |
| `listing_ttl` | `"10m"` | Cache TTL for issue query scopes |
| `issue_ttl` | `"5m"` | Cache TTL for single-issue refreshes (`issue show`, `watch check`) |
| `github_ttl` | `"30m"` | Cache TTL for GitHub issue and timeline checks |
| `overlap` | `"10m"` | Lookback overlap for incremental `updatedAt` syncs |
| `full_interval` | `"24h"` | Interval for automatic full resyncs when within request budget |
| `max_requests` | `50` | Default per-invocation HTTP request budget |
| `wave_rate` | `2.0` | Token-bucket rate limit (requests/sec) for `wave-api.drips.network` |
| `wave_burst` | `4` | Token-bucket burst size for `wave-api.drips.network` |
| `high_value_min_points` | `200` | Minimum points used by the built-in `high-value` preset |
| `hidden_presets` | `[]` | Built-in preset names hidden by `dwave preset rm` |

### GitHub Cross-Checking & Caveats

- **Token resolution**: When a GitHub API request is needed, `dwave` uses the first nonempty value from `GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token --hostname github.com`. Without a token it makes anonymous requests (60 requests/hour) and emits a warning.
- **Conditional caching**: `--github` fetches `/repos/{owner}/{repo}/issues/{n}` and `/timeline` using `If-None-Match` (`ETag` / `304 Not Modified`) and caches results in SQLite.
- **Cold reference lookups**: Resolving an uncached `OWNER/REPO#N` or GitHub issue URL may fetch the public GitHub issue title even without `--github` to narrow Wave's search before verifying an exact repo/number match. `--offline` suppresses that lookup.
- **Wave snapshot caveats**: Wave data can change between requests, and a missing Wave `prLink` does not prove no PR exists on GitHub. Always check `--github` and verify availability on Wave and GitHub before applying.

## Companion Agent Skill & Rust Library

- **Agent Skill**: [`skills/dwave/SKILL.md`](https://github.com/freesialuo/dwave/blob/main/skills/dwave/SKILL.md) provides concise rules and JSON recipes for AI coding agents driving `dwave`.
- **Rust Library (`wave-core`)**: [`crates/wave-core`](https://github.com/freesialuo/dwave/tree/main/crates/wave-core) exposes the synchronous `Engine`, `HttpWaveSource`, `Gateway` (budget, rate limiting, retries), SQLite `Store`, `Watcher`, `GitHubClient`, and typed `Issue`, `IssueState`, `Program`, `Repo`, `Wave`, and `WaveStatus` projections (`TryFrom<&serde_json::Value>`) that retain unrecognized fields in `extra`.

## Development

The Cargo workspace separates [`crates/wave-core`](https://github.com/freesialuo/dwave/tree/main/crates/wave-core) from the [`crates/dwave`](https://github.com/freesialuo/dwave/tree/main/crates/dwave) CLI binary. All CI tests run offline against recorded fixtures rather than the live Wave API:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
node --test scripts/npm-release.test.mjs
```

Changes to the `dwave/v1` JSON structure should preserve backwards compatibility for scripts and the companion skill.

## Releasing

Tag pushes matching `vX.Y.Z` or `vX.Y.Z-<prerelease>` (without build metadata) trigger [`.github/workflows/release.yml`](https://github.com/freesialuo/dwave/blob/main/.github/workflows/release.yml):

1. Update `[workspace.package].version` in `Cargo.toml` and `"version"` in `npm/package.json` to the same SemVer string, refresh `Cargo.lock` (`cargo check --workspace`), and run the checks above plus:
   ```sh
   node scripts/npm-release.mjs validate vX.Y.Z
   ```
2. Create a signed commit and matching signed tag, then push both to GitHub:
   ```sh
   git tag -s vX.Y.Z -m "vX.Y.Z"
   git push origin main vX.Y.Z
   ```
3. GitHub Actions builds and smoke-tests all five native targets (`linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`), packs `@freesialuo/dwave`, verifies archive contents and SHA-512 integrity, uploads the `npm-package` artifact, and publishes via npm OIDC trusted publishing (`NPM_TRUSTED_PUBLISHING_ENABLED=true`, `npm` environment) with SLSA provenance—using dist-tag `latest` for stable versions and `next` for prereleases. Rerunning a tag workflow succeeds as a no-op only if the registry tarball integrity matches byte-for-byte.
4. The `npm` environment requires a maintainer's approval: the publish job waits until a required reviewer approves it under **Review deployments** on the workflow run page.
5. After publishing, npm may take several minutes to process the release. The version metadata can appear before the tarball, which may return `404` until processing finishes; confirm with `npm view @freesialuo/dwave@X.Y.Z dist.integrity` and a fresh `npm pack @freesialuo/dwave@X.Y.Z`.

## License

Licensed under either the [MIT License](https://github.com/freesialuo/dwave/blob/main/LICENSE-MIT) or the [Apache License, Version 2.0](https://github.com/freesialuo/dwave/blob/main/LICENSE-APACHE), at your option.
