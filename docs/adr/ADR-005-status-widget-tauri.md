# ADR-005: Status widget on Tauri v2 + Svelte

- Status: accepted (implementation v0.6)
- Date: 2026-09-16
- Supersedes: the AppKit `pereprava-menubar` crate (removed)

## Context

The first status UI was a hand-rolled AppKit menu-bar item (`objc2` FFI). It
worked, but every visual change meant writing Objective-C message sends by
hand, the whole crate lived in `unsafe`, and it could not grow beyond a title
and three menu lines. A dashboard (transfer history, per-storage view,
thresholds) was out of reach.

The watch daemon already publishes a machine-readable contract —
`/tmp/pereprava-status.json`, rewritten atomically on every poll — so the UI is
free to be anything that can read a file.

## Decision

Build the status UI as a **Tauri v2 application with a Svelte 5 frontend**
(`crates/widget`), replacing the AppKit menu bar item.

- **Menu-bar presence** comes from Tauri's tray icon (`tray-icon` feature),
  using a monochrome template glyph so macOS tints it for light/dark.
- **Dashboard** is a compact Svelte window (380×540) with live rx/tx rates,
  cumulative totals, mount point and open/unmount actions.
- **Rust side owns no MTP logic.** It reads the status file, re-emits it to
  the frontend as a `status` event once a second, and shells out to `open` /
  `umount` for the two actions — exactly what the CLI offers.
- The app runs as an **accessory** (`ActivationPolicy::Accessory`), so it has
  no Dock icon.

## Consequences

- UI iteration is HTML/CSS/Svelte — no FFI, no `unsafe` in the view layer.
- `crates/widget` is a **standalone Cargo workspace**: `tauri build` requires
  the built frontend (`ui/dist`) and must not be dragged into
  `cargo build --workspace`.
- Added build prerequisites: Node + `@tauri-apps/cli` for the frontend; the
  Rust side stays pure (`tauri`, `serde`, `serde_json`).
- The status-file contract from ADR-004/`watch` is now load-bearing for two
  consumers (widget + humans) and should be treated as a small public API.
