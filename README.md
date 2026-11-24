# dwave

An unofficial, read-only command-line tool for discovering and tracking issues in Drips Wave programs. It is not affiliated with Drips. The Wave API is undocumented and may change without notice.

`dwave` combines issue discovery, a local cache, and an on-demand watchlist. It is intended for contributors who want to find work and follow changes without repeatedly scanning an entire program. A reusable Rust library lives alongside the CLI.

## Getting started

Install the CLI from npm with Node.js 22 or newer:

```sh
npm install -g @freesialuo/dwave
dwave --help
```

The npm package targets macOS arm64/x64, Linux glibc arm64/x64 (Ubuntu 22.04 baseline), and Windows x64. To build and run it from a Rust checkout:

```sh
cargo run --release -- --help
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

`issues --new-since-last` seeds its baseline on the first complete run and returns no new matches then. Later runs retain matches that have not yet been delivered across pages. Both discovery and `watch check` return 10 for confirmed changes, 0 for a complete check with no changes, 1 for an error or a partial check without confirmed changes, and 2 for usage errors. A partial check with confirmed changes still returns 10, so always inspect `meta.partial` and warnings. A scheduled job must account for 10 as a change signal. Use `watch log` to inspect history.

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

The client makes read-only requests to Wave and GitHub. Resolving an uncached `owner/repo#N` or GitHub issue URL may request the public GitHub issue title even without `--github`, then match the Wave repository and issue number exactly. `--offline` suppresses that lookup. It does not apply to issues or post comments. Watch checks run on demand; use your own scheduler if you want periodic checks.

## Development

The Cargo workspace separates the `wave-core` library from the `dwave` binary. Run the standard checks from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests should use fixtures rather than depend on the live Wave API. Changes to the JSON output used by scripts and the skill should be treated as compatibility changes.

## Releasing

The release workflow builds and tests native binaries and uploads the tested `npm-package` tarball for a matching release tag. Publishing is disabled until the repository Actions variable `NPM_TRUSTED_PUBLISHING_ENABLED` is set to `true`.

### One-time setup

1. Confirm that the publishing account owns the `@freesialuo` scope or has permission to publish [@freesialuo/dwave](https://www.npmjs.com/package/@freesialuo/dwave). Create an npm account, verify its email, and [enable two-factor authentication](https://docs.npmjs.com/configuring-two-factor-authentication/) for the maintainer account.
2. In the GitHub repository, enable Actions and create an environment named `npm` under **Settings → Environments**. Configure any desired reviewers and deployment restrictions; if using selected branches and tags, explicitly allow the release tags. Availability of environments and protection rules depends on repository visibility and plan; see [GitHub's environment documentation](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments). Protect release tags as appropriate.
3. For a new scoped release, download the `npm-package` artifact from its completed tag run. The pack job stages the native binaries and runs `npm pack` on the staged package. Compare the downloaded tarball's SHA-512 integrity with the `integrity` output in that run's **Verify archive contents, metadata, and integrity** step. This Node.js command works on macOS, Linux, and Windows (substitute the downloaded path):

   ```sh
   node -e "const fs=require('node:fs'),c=require('node:crypto'); console.log('sha512-'+c.createHash('sha512').update(fs.readFileSync(process.argv[1])).digest('base64'))" /path/to/freesialuo-dwave-0.1.0.tgz
   ```

   Proceed only if it matches. The original `v0.1.0` Actions artifact was unscoped. Its locally repacked scoped tarball, `freesialuo-dwave-0.1.0.tgz`, contains the same binaries but has a different digest; do not compare it to the original artifact's integrity as if it were the same archive. On a machine logged in as the npm maintainer, publish the verified scoped tarball, responding to the 2FA prompt:

   ```sh
   npm login
   npm publish /path/to/freesialuo-dwave-0.1.0.tgz --access public
   ```

   Replace the filename with the actual tarball. For a prerelease, add `--tag next`. Check the package name and version before publishing: npm versions cannot be reused. If npm stages a release for approval, an authorized package maintainer must approve it before it becomes publicly available. The existing `0.0.0-stage` placeholder establishes the package; use an unpublished version for the first trusted publish.
4. When the next unpublished release is ready, configure [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/) in the package's **Settings → Trusted publishing**. Select GitHub Actions, enter the actual GitHub owner and repository, workflow filename `release.yml` (filename only), and environment `npm`; enable direct `npm publish`. In GitHub, set the repository variable `NPM_TRUSTED_PUBLISHING_ENABLED` to `true` under **Settings → Secrets and variables → Actions → Variables**. The first successful publish with a new trusted publisher must occur within two days, or the configuration expires and must be recreated. A matching existing version is a no-op, so use a new version to validate OIDC. After a successful trusted publish, consider restricting token-based publishing as npm recommends.

### Each release

1. Update the workspace package version in `Cargo.toml` and the version in `npm/package.json` to the same SemVer version. Refresh `Cargo.lock` and run the development checks above.
2. Commit the version updates and push a matching tag such as `v1.2.3` or `v1.2.3-rc.1`. The tag must not include build metadata. GitHub Actions builds and tests the native binaries, verifies the tarball, and uploads `npm-package`. When publishing is enabled, it publishes stable versions under `latest` and prereleases under `next` through OIDC.
3. Check the Actions run and the published package. A rerun accepts an existing version only when its registry integrity matches the tested tarball; a different tarball fails. A successful no-op does not validate trusted publishing or change a dist-tag. Trusted publishing uses GitHub-hosted runners, and npm automatically generates provenance for a public package published from a public repository.

No package is published merely by preparing this repository.

## License

Licensed under either the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
