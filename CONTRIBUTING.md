# Contributing to okeanos

Thanks for your interest. okeanos is early and fast-moving, so please open an issue to discuss larger changes before starting.

## Setup

```sh
git clone https://github.com/edu-diez/okeanos
cd okeanos
cargo build
```

The Rust toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml).

## Before opening a PR

```sh
just check   # cargo fmt --check, cargo clippy -- -D warnings, cargo test
```

CI additionally runs clippy with `--all-targets --all-features` on several operating systems, so consider running that locally too.

## Guidelines

- Read [docs/architecture.md](docs/architecture.md) first. Key rule: the harness library never depends on `ui`, `headless` or `cli`.
- The TUI and headless modes are renderers of the same event stream. Don't add UI-only state; see [docs/design/tui-design.md](docs/design/tui-design.md).
- No C dependencies in the primary dependency tree.
- Add or update tests for behaviour you change. Harness logic is tested with the mock provider (`mock-provider` feature).
- Add an entry to [CHANGELOG.md](CHANGELOG.md) for user-visible changes.
- Instructions for AI agents working on this repo are in [AGENTS.md](AGENTS.md).

## Licensing

okeanos is licensed under [AGPL-3.0-or-later](LICENSE). By contributing you agree that your contribution is released under the same license.

## Reporting security issues

Please don't open a public issue for vulnerabilities. Contact the maintainer privately through GitHub.
