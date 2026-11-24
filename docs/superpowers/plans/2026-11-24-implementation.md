# dwave implementation and acceptance plan

The repository starts with the design document alone. Target the first CLI and npm package at version 0.1.0. Use a synchronous library interface for this on-demand CLI so retries and the invocation-wide request budget have a single owner.

| Local branch | Responsibility | Review gate |
| --- | --- | --- |
| feature/source | Cargo workspace, tolerant Wave models, read-only source, HTTP transport | Fixture decoding and per-attempt budget, including retries |
| feature/engine | SQLite, filter planning, scoped cache and pagination | Repeat and offline reads, scope membership, resumable partial runs |
| feature/watch | Watch history and optional GitHub enrichment | Stable baselines, genuine change events and conditional requests |
| feature/cli | Command tree, config and presets, human and machine output | Executable tests of normal commands and exit codes |
| feature/release | Licenses, cross-platform CI and npm packaging | Test the installed package, validate version alignment, isolate publish permissions |
| feature/docs | README and companion skill | Check examples against executable help and the actual release workflow |

Integrate onto integration/dwave. Keep feature commits for review. Verify the assembled workspace using rustfmt, Clippy and fixture-based tests, then exercise the installed npm command on the host. Review public documentation and tag-triggered publishing separately. Remote configuration, package publication and tag pushes belong to the repository owner after local acceptance.

The API's issue timestamps are insufficient evidence of assignment or application changes. Refresh watched IDs directly and retain the conservative full-sync interval. Treat a truncated sync differently from a completed empty result.
