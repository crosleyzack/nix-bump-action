# nix-bump-action

A GitHub Action that bumps nixpkgs and home-manager release pins in your `*.nix` files.

It rewrites files and nothing else.
Re-locking is done with existing [`update-flake-lock`](https://github.com/DeterminateSystems/update-flake-lock) action.

## Usage

```yaml
on:
  schedule:
    - cron: '0 6 * * *'
  workflow_dispatch:

jobs:
  bump:
    runs-on: ubuntu-slim
    permissions:
      contents: write
      pull-requests: write
    steps:
      - uses: actions/checkout@v7

      - uses: crosleyzack/nix-bump-action@v0.1.0
        with:
          paths: nix

      - uses: DeterminateSystems/determinate-nix-action@v3
      - uses: DeterminateSystems/update-flake-lock@v27
        with:
          path-to-flake-dir: nix
```

Rewriting the ref leaves `flake.lock` stale, which is the case `update-flake-lock` resolves.
It takes one `path-to-flake-dir`, so several flakes need one step each.

## What it rewrites

| Pattern | Becomes |
| --- | --- |
| `github:nixos/nixpkgs/nixos-XX.YY` | `nixos-<target>` |
| `github:nix-community/home-manager/release-XX.YY` | `release-<target>` |

The owner is matched case-insensitively, so `github:NixOS/nixpkgs/...` works.
A pin already at or past the target is left alone, so the action is idempotent and never downgrades.

`stateVersion` and `flake.lock` are out of scope.
`stateVersion` records the release your state was initialized under, not the release you run, and [changing it does not upgrade anything](https://mynixos.com/nixpkgs/option/system.stateVersion).

## The target release

The target is the newest home-manager `release-XX.YY`, not the newest nixpkgs `nixos-XX.YY`.
home-manager branches only after nixpkgs, so its newest release can never run ahead.
This makes "wait for home-manager" structural rather than a computed gate.
The trade-off is that a repository with only nixpkgs pins still waits for home-manager.

## Inputs

| Name | Default | Description |
| --- | --- | --- |
| `paths` | `.` | Newline-separated files and directories. Directories are searched recursively for `*.nix`; files are taken as given. A path that does not exist is an error. |
| `target-version` | | Bump to this `XX.YY` instead of discovering the newest. |
| `dry-run` | `false` | Report what would change without writing. |
| `home-manager-repo` | `nix-community/home-manager` | Repository read for `release-*` branches. |

## Outputs

`latest-home-manager`, `target`, `bumped`, `files-changed`, `versions-bumped`.

## Development

The action builds a Rust binary with `cargo`, which is preinstalled on `ubuntu-latest` and `macos-latest`.

```sh
cargo test
cargo clippy --all-targets -- -D warnings

# --target-version skips the upstream lookup, so this needs no network.
cargo run -- --target-version 26.05 --dry-run ./fixture
```

## License

MIT
