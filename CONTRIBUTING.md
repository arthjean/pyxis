# Contributing to Numen

Numen is early and moving fast. Issues, bug reports, and focused pull requests are all welcome.

## Before you start

- Open an issue for anything non-trivial before sending a PR, so we can agree on direction first.
- Keep PRs scoped: one logical change, atomic [conventional commits](https://www.conventionalcommits.org/).

## Development

- Rust 1.95+ (edition 2024). Build and test the whole workspace:
  ```bash
  cargo build --workspace
  cargo test --workspace
  ```
- The workspace enforces clippy lints: `panic`, `unimplemented`, and `dbg!` are denied; `unwrap`/`expect` are warnings. Avoid them in non-test code (prefer `?`, `ok_or(...)`, `match`). Run `cargo clippy --workspace --no-deps` and `cargo fmt` before pushing.
- Respect the architecture invariants in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). In particular, `agent-core` stays headless: it must not depend on `agent-tui` or `agent-provider`, and it must never emit ANSI.

## Licensing of contributions

Numen is licensed under **GPL-3.0-or-later** (see [`LICENSE`](LICENSE)) and is also offered by StriveX under separate commercial terms. To keep both possible, contributions are accepted under the following terms:

- You license your contribution under **GPL-3.0-or-later**, the project's open-source license.
- You also grant **StriveX (Arthur Jean)** a perpetual, worldwide, non-exclusive, royalty-free right to use, reproduce, modify, and **relicense your contribution, including under commercial license terms**, as part of Numen.
- You keep the copyright to your contribution. This is a license grant, not an assignment.

If you would rather not grant the commercial-relicensing right, say so in your PR: small fixes can still be merged under plain GPL-3.0-or-later on a case-by-case basis.

## Developer Certificate of Origin

Every commit must be signed off, certifying the [Developer Certificate of Origin 1.1](https://developercertificate.org/):

```bash
git commit -s -m "feat(scope): your change"
```

The `-s` flag appends a `Signed-off-by: Your Name <your@email>` line. By signing off, you certify that you wrote the contribution (or otherwise have the right to submit it) and that you agree to the licensing terms above.
