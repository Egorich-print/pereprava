# Contributing

## Ground rules

1. **No `unsafe`.** First-party code forbids it (`#![forbid(unsafe_code)]`).
2. **No `unwrap()`/`expect()`/`panic!`** in library or CLI code — CI denies
   the lints. Propagate typed errors instead.
3. Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `bench:`...).
4. Public items need doc comments (`missing_docs` is on).
5. Every user-visible change gets a CHANGELOG entry.

## Development

```shell
cargo build
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# device-gated integration tests (Nothing Phone 2 connected, MTP mode):
PEREPRAVA_DEVICE=1 cargo test -p pereprava-core --test device -- --ignored --test-threads=1
```

## Docs to update when behavior changes

- `STATUS.md` — current state (single source of truth).
- `CHANGELOG.md` — what changed.
- `docs/status/*.md` — feature notes / state snapshots.
