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

Filters that require local evaluation can take more requests than server-filtered queries. Use `--explain` to inspect the plan, narrow the query where possible, and adjust `--max-requests` deliberately. `--offline` uses cached data; `--fresh` requests a refresh. JSON output includes a `dwave/v1` schema and metadata such as freshness, request count, warnings, and whether results are partial. Check that metadata before relying on a result. `--ndjson` is available for streaming records.

`watch check` returns exit code 10 when changes are found, 0 when none are found, 1 on error, and 2 for usage errors. A scheduled job must account for 10 as a change signal. Use `watch log` to inspect history.

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

The release workflow builds and tests native binaries and packages them for npm. Push a SemVer tag such as `v1.2.3` or `v1.2.3-rc.1` to run it; stable releases use the npm `latest` tag and prereleases use `next`.

The first npm publication requires a maintainer with npm 2FA to download the tested `npm-package` artifact from the tag run and publish its tarball with `npm publish /path/to/dwave-0.1.0.tgz --access public` (use the actual artifact name). Configure npm trusted publishing with the actual GitHub owner and repository, workflow `release.yml`, and environment `npm` before subsequent releases. Later publications use OIDC. If the OIDC job started before the initial publication, rerun the failed job after bootstrapping. No package is published merely by preparing this repository.

## License

Licensed under either of the MIT License or the Apache License, Version 2.0, at your option.
