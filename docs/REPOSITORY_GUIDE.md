# Repository Guide

Layout follows the Knowledge/System project template.

```
pereprava/
├── Cargo.toml            # workspace: edition 2024, rust-version 1.98, shared lints
├── crates/
│   ├── core/             # pereprava-core: MTP device actor, cache, model
│   └── cli/              # pereprava binary
├── docs/
│   ├── adr/              # architecture decision records (source of truth)
│   ├── status/           # feature notes & state snapshots (.md)
│   └── benchmarks/       # measurement methodology + results
├── STATUS.md             # single source of truth for project status
├── CHANGELOG.md
└── CONTRIBUTING.md
```

- Canonical location: `~/ai-workstation/Projects/pereprava`
  (Execution layer; hub symlink: `New OpenCode Project/projects/pereprava`).
- Git: Conventional Commits; tags `vX.Y.Z`; baseline tag after first stable CLI.
- Binaries/artifacts never enter git.
