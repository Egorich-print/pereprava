# Repository Guide

Layout follows the Knowledge/System project template (Active Development
skeleton).

```
pereprava/
├── Cargo.toml            # workspace: edition 2024, rust-version 1.98, shared lints
├── crates/
│   ├── core/             # pereprava-core: MTP device actor, cache, model
│   ├── cli/              # pereprava binary (mount, watch, pack, ...)
│   ├── nfs-mount/        # pereprava-nfs: NFSv3 adapter + mount automation
│   └── widget/           # Tauri v2 + Svelte status widget (own workspace)
├── docs/
│   ├── REPOSITORY_GUIDE.md   # this file
│   ├── architecture/         # system overview + data flow
│   ├── adr/                  # architecture decision records (source of truth)
│   ├── history/              # dated feature notes & state snapshots
│   └── benchmarks/           # measurement methodology + results
├── scripts/              # daemon control, E2E harnesses, installer
├── vendor/fernfs/        # vendored NFS server (BSD-3), patched locally
├── STATUS.md             # single source of truth for project status
├── CHANGELOG.md
└── CONTRIBUTING.md
```

## Layers (Knowledge/System)

| Layer | Location | Content |
|-------|----------|---------|
| Execution | `~/ai-workstation/Projects/pereprava` | This repository (code + local docs) |
| Knowledge | `Obsidian Vault/Projects/Pereprava` | Overview, Decisions, Links, `ADR/` symlink → `docs/adr` |
| Showcase | `~/Documents/Проекты/pereprava` | Public README + STATUS |

Hub access is by symlink only: `New OpenCode Project/projects/pereprava`.

## Conventions

- Git: Conventional Commits; tags `vX.Y.Z`.
- Binaries/artifacts never enter git (`target/`, `ui/dist`, `ui/node_modules`).
- ADRs are immutable once accepted; supersede with a new one.
- First-party code forbids `unsafe` and denied `unwrap`/`expect`/`panic`
  lints (ADR-000). The widget crate is the only exception (platform glue).
