# dwave

An unofficial, read-only command-line tool for discovering and tracking issues in Drips Wave programs. It is not affiliated with Drips. The Wave API is undocumented and may change without notice.

`dwave` combines issue discovery, a local cache, and an on-demand watchlist. It is intended for contributors who want to find work and follow changes without repeatedly scanning an entire program. A reusable Rust library lives alongside the CLI.

## Getting started

Once the package is published, install it with Node.js 22 or newer:

```sh
npm install -g dwave
dwave --help
```

The npm package targets macOS arm64/x64, Linux glibc arm64/x64 (Ubuntu 22.04 baseline), and Windows x64. Until publication, build from a Rust checkout:

```sh
cargo build --release
./target/release/dwave --help
```

Use `dwave <command> --help` for the available options. The CLI can emit machine-readable JSON for scripts and the companion [skill](skills/dwave/SKILL.md).

## Discover and track issues

```sh
# Choose a program and inspect its waves.
dwave program use stellar
dwave waves --current

# Browse a server-filtered preset or narrow a query.
dwave issues --preset untouched --limit 20
dwave issues --repo owner/repo --pending 0 --no-pr --explain

# Inspect an issue, then add it to the local watchlist.
dwave issue show 'owner/repo#123' --github
dwave watch add 'owner/repo#123'
dwave watch list
dwave watch check --json
```

Replace the example repository and issue number with real values. `untouched` selects issues that have never had an application and have no Wave PR link. `--pending 0` is broader: it can include issues with earlier withdrawn or rejected applications. Other presets include `open-pending-0`, `unassigned`, `high-value`, and `repo`; inspect them with `dwave preset list`.

Filters that require local evaluation can take more requests than server-filtered queries. Use `--explain` to inspect the plan and cached estimate without making requests; narrow the query where possible, and adjust `--max-requests` deliberately. `--offline` uses cached data; `--fresh` requests a refresh. JSON output includes a `dwave/v1` schema and metadata such as freshness, request count, warnings, and whether results are partial. Check that metadata before relying on a result. `--ndjson` is available for streaming records.

`issues --new-since-last` seeds its baseline on the first complete run and returns no new matches then. Later runs return exit code 10 when new matches are found, even if the requested page is empty. `watch check` returns exit code 10 when changes are found, 0 when none are found, 1 on error, and 2 for usage errors. A scheduled job must account for 10 as a change signal. Use `watch log` to inspect history.

For scheduled discovery, replace `watch check` in the following example with `issues --preset untouched --new-since-last`. For either command, handle the change exit code explicitly. This POSIX shell example also works with `set -e` because the command is in an `if` condition:

```sh
if dwave --json watch check > watch-changes.json; then
  echo "No changes"
else
  status=$?
  case "$status" in
    10) echo "Changes found; inspect watch-changes.json" ;;
    *) echo "Watch check failed ($status)" >&2; exit "$status" ;;
  esac
fi
```

## Configuration and GitHub access

`dwave config get <key>` and `dwave config set <key> <value>` inspect and update the TOML configuration. For example, `dwave config set listing_ttl 10m` adjusts the listing freshness window. Durations use `s`, `m`, `h`, `d`, or `w`. The default program is `stellar`; change it with `dwave program use <slug>` or override it for one invocation with `--program <slug>`. For config and database paths, explicit `--config PATH` and `--db PATH` take precedence over `DWAVE_CONFIG` and `DWAVE_DB`, respectively, followed by platform defaults. `--max-requests` overrides the configured budget. Explicit filters override preset fields; repeated `--repo` or `--label` options replace the corresponding preset list. An explicit slug on `program show` takes precedence over `--program`.

GitHub enrichment uses the first nonempty `GITHUB_TOKEN`, `GH_TOKEN`, or token returned by `gh auth token --hostname github.com`, in that order. Without a token it makes anonymous requests and warns about the lower quota. Tokens are resolved only when a GitHub network request is needed. In offline mode, watch checks and GitHub enrichment use cached observations; stale data is reported, and missing observations produce partial results and warnings. Check the output metadata before treating a cross-check as complete.

## Working with Wave

Wave's issue information can change between requests. Cached data is a snapshot, not a guarantee that an issue is still available. Before applying, check the issue on Wave and GitHub. In particular, a missing Wave PR link does not prove that no pull request exists.

The client makes read-only requests to Wave and, when requested, GitHub. It does not apply to issues or post comments. Watch checks run on demand; use your own scheduler if you want periodic checks.

## Development

The Cargo workspace separates the `wave-core` library from the `dwave` binary. Run the standard checks from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests should use fixtures rather than depend on the live Wave API. Changes to the JSON output used by scripts and the skill should be treated as compatibility changes.

## Releasing

The release workflow builds and tests native binaries and packages them for npm. To prepare a release:

1. Update the workspace package version in `Cargo.toml` and the version in `npm/package.json` to the same SemVer version; refresh `Cargo.lock` and run the development checks above.
2. Commit the version updates, then push a matching tag such as `v1.2.3` or `v1.2.3-rc.1`. The workflow builds the `npm-package` artifact and publishes stable releases under `latest` and prereleases under `next`.
3. For the first publication, an npm maintainer with 2FA must download the tested `npm-package` Actions artifact from that tag run and publish the contained tarball: `npm publish /path/to/dwave-0.1.0.tgz --access public` (substitute the actual filename; add `--tag next` for a prerelease). If the OIDC job started before bootstrap, rerun the failed job after manual publication; an identical published tarball is accepted as a no-op, so this does not validate trusted publishing.
4. Configure [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/) with the actual GitHub owner and repository, workflow filename `release.yml`, environment `npm`, and permission for direct `npm publish`. The first automated publish using a new configuration must succeed within two days, or the configuration expires and must be recreated. Configure it when the next release is ready. Subsequent releases use OIDC.

Protect the GitHub environment and release tags as appropriate. Once OIDC works, npm recommends restricting token-based publishing. No package is published merely by preparing this repository.

## License

Licensed under either the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
