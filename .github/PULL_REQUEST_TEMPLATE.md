# Pull Request

## Description

<!-- Provide a clear description of the changes in this PR -->

## Crate(s) Affected

<!-- Check all that apply -->

- [ ] `bogdan-server`
- [ ] `bogdan-protocols`
- [ ] `bogdan-session`
- [ ] `bogdan-resolver`
- [ ] `bogdan-playback`
- [ ] `bogdan-display`
- [ ] `bogdan-tor`
- [ ] Workspace / Cross-cutting

## Type of Change

<!-- Check the relevant option -->

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds functionality)
- [ ] Breaking change (fix or feature that would cause existing functionality to not work as expected)
- [ ] Refactoring (no functional changes)
- [ ] Documentation update
- [ ] CI/CD change
- [ ] Configuration change

## Testing

<!-- Describe the testing that was performed -->

- [ ] Unit tests pass (`cargo test --workspace`)
- [ ] Manual testing on device/emulator
- [ ] Cross-compilation for aarch64 verified

### Test Details

<!-- Provide additional context about testing — what was tested, how, and results -->

## Checklist

- [ ] `cargo check` passes with no errors
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo fmt --all -- --check` passes
- [ ] Documentation has been updated (README, crate docs, ARCHITECTURE.md, etc.)
- [ ] No new Clippy warnings introduced
- [ ] Changes are backwards-compatible (or breaking change is documented)

## Related Issues

<!-- Link any related issues: Closes #123, Fixes #456 -->
