# Pull Request

## Summary

<!-- One paragraph: what does this PR do and why? -->

## Type of change

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds functionality)
- [ ] Breaking change (fix or feature that changes existing behaviour)
- [ ] Refactor / cleanup (no behaviour change)
- [ ] Documentation / CI / tooling

## Related issues

Closes #<!-- issue number -->

## Test plan

<!-- Describe how you tested this change. -->

- [ ] New unit/integration test added that covers this change.
- [ ] Existing tests still pass (`cargo test --workspace --all-features --lib`).
- [ ] For write-path changes: chaos suite run (`APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test chaos_write`).
- [ ] Tested on a real APFS volume (state source: USB / image / macOS version).

## Checklist

- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --workspace --all-features -- -D warnings` passes (zero warnings).
- [ ] `cargo doc --workspace --no-deps` builds without errors.
- [ ] No `unwrap()` / `expect()` / `panic!()` added in library code paths.
- [ ] No new magic numbers — named `const` used.
- [ ] `CHANGELOG.md` updated under `[Unreleased]` for user-visible changes.
- [ ] Breaking changes documented in `CHANGELOG.md`.

## On-disk format changes (if applicable)

<!-- If this PR touches APFS on-disk structures, complete this section. -->

- [ ] At least two independent reference sources consulted (list them below).
- [ ] Python redecode cross-check performed for offset-sensitive paths.

**Sources consulted:**

1.
2.

## Additional notes

<!-- Anything reviewers should know: performance impact, rollback plan, etc. -->
