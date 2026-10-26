# Release process

Releases are automated with [release-please](https://github.com/googleapis/release-please) and the
[`release.yml`](.github/workflows/release.yml) workflow. Nobody tags or pushes images by hand.

## Flow

1. **Use conventional commits.** [`conventional_commits.yml`](.github/workflows/conventional_commits.yml)
   runs `cz check` over every commit in a PR, so each commit message needs a prefix such as `feat:`,
   `fix:` or `feat!:`. The prefix decides the version bump.
2. **Merge to `main`.** The `release-please` job opens or updates a `chore: release main` PR. The
   workspace has one version, so that PR bumps `[workspace.package].version` in `Cargo.toml`, the
   two crate entries in `Cargo.lock`, `CHANGELOG.md` and [`.release-please-manifest.json`](.release-please-manifest.json).
3. **Merge the release PR.** release-please creates the git tag and GitHub release, for example
   `v0.2.0`.
4. **The image is built in the same workflow run.** `graph-monitor` is built from
   [`docker/Dockerfile.graph-monitor`](docker/Dockerfile.graph-monitor) for `linux/amd64` and
   `linux/arm64`, then merged into one multi-arch manifest at `ghcr.io/graphprotocol/graph-monitor`.

To ship: merge conventional-commit PRs, then merge the release-please PR. The versioned image appears
on GHCR a few minutes later.

## Image tags

| Trigger | Tags |
|---|---|
| Push to `main` that cuts a release | `X.Y.Z`, `X.Y`, `X`, `vX.Y.Z`, `vX.Y`, `vX`, `main`, `sha-<short>` |
| Any other push to `main` | `main`, `sha-<short>` |
| PR from a branch in this repo | `pr-<N>`, `sha-<short>` |
| PR from a fork | Built, not pushed |
| `workflow_dispatch` | `<branch>`, `sha-<short>` (never a version tag) |

The image is built on every run, whether or not a release was cut. Only the version tags depend on
the release. `:main` always tracks the head of `main`.

## Running the image

The entrypoint is the `graph-monitor` binary and the default command is `serve`, which exports
Prometheus metrics on port 9090. The shipped config is compiled into the binary, so the image runs
with no config file. Override it with a mounted file or environment variables:

```sh
docker run -p 9090:9090 \
  -e GRAPH_MONITOR_RPC_URL=https://... \
  -v ./graph-monitor.toml:/etc/graph-monitor.toml -e GRAPH_MONITOR_CONFIG=/etc/graph-monitor.toml \
  ghcr.io/graphprotocol/graph-monitor:main
```

Any other subcommand works too, for example `docker run ghcr.io/graphprotocol/graph-monitor:main status`.
