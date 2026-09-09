## Summary

<!-- What does this PR do, in one or two sentences? -->

## Changes

-

## Testing

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] New/changed behavior covered by tests or a benchmark

## Privacy checklist

- [ ] No network calls added; local surfaces still bind to 127.0.0.1
- [ ] Raw event layer stays append-only (no truncation/re-keying)
- [ ] Input data remains per-minute counts by default

## Docs

- [ ] `CHANGELOG.md` (Unreleased) updated if user-visible
- [ ] `BENCHMARKS.md` / `APP.md` updated if numbers or behavior changed
