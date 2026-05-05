# Contributing to PiCast

Thank you for your interest in contributing! This guide covers everything you
need to get started.

## Prerequisites

| Tool | Minimum Version | Notes |
|---|---|---|
| Rust | 1.70+ | Install via [rustup](https://rustup.rs) |
| `cross` | latest | For cross-compilation to Raspberry Pi (`cargo install cross`) |
| `cargo-deny` | latest | For dependency auditing (`cargo install cargo-deny`) |
| `cargo-audit` | latest | For vulnerability scanning (`cargo install cargo-audit`) |
| Make | any | Optional, for using the `Makefile` targets |

## Development Workflow

1. **Fork** the repository on GitHub.
2. **Create a feature branch** from `main`:
   ```bash
   git checkout -b feat/my-feature
   ```
3. **Make your changes** with clear, focused commits.
4. **Push** your branch and open a **Pull Request** against `main`.
5. Address review feedback and ensure all CI checks pass.

## Commit Convention

We follow [Conventional Commits](https://www.conventionalcommits.org/). Each
commit message should be structured as:

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

**Types:** `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`

**Examples:**
```
feat(playback): add ABR controller for adaptive bitrate selection
fix(tor): resolve stream isolation leak on reconnect
docs(protocols): document WebSocket message envelope schema
```

## Code Style

- **Formatting:** All code must pass `cargo fmt --check` (configuration in
  `rustfmt.toml`).
- **Linting:** All code must pass `cargo clippy -- -D warnings` with zero
  warnings (configuration in `clippy.toml`).
- **Dependencies:** `openssl` and `curl` are banned; use `rustls` and `hyper`
  instead.

## Testing Requirements

- Every new feature or bug fix **must** include tests.
- Unit tests live in the same file as the code they test (`#[cfg(test)]`).
- Integration tests belong in the crate's `tests/` directory.
- All tests must pass before a PR can be merged:
  ```bash
  cargo test
  ```

## Documentation Requirements

- All **public** items (functions, structs, enums, traits, modules) must have
  doc comments with examples where applicable.
- Use `///` for item-level docs and `//!` for module-level docs.
- Run `cargo doc --no-deps` and verify no warnings.

## PR Checklist

Before submitting your pull request, confirm the following:

- [ ] Commits follow the conventional commits format
- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy -- -D warnings` passes with zero warnings
- [ ] `cargo test` passes
- [ ] New public items have doc comments
- [ ] No `openssl` or `curl` dependencies introduced
- [ ] `cargo deny check` passes (licenses & bans)

---

Questions? Open a [Discussion](https://github.com/pkhairkh/picast/discussions) or
reach out in the issue tracker. Happy hacking!
