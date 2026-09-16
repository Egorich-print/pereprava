# Changelog

All notable changes to this project will be documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/).

## [Unreleased]

### Changed (v0.6)

- **Status widget rewritten on Tauri v2 + Svelte** (`crates/widget`),
  replacing the AppKit `pereprava-menubar` crate (ADR-005). Tray icon with a
  template bridge glyph plus a Svelte dashboard: connection state, model,
  live rx/tx rates, cumulative totals, mount point, open/unmount actions.
  Rust side reads the status file and emits `status` events; no MTP logic.
- `crates/widget` is a standalone Cargo workspace (Tauri needs the built
  `ui/dist`), excluded from `cargo build --workspace`.
- Installer builds and installs `Pereprava.app` under `~/Applications` and
  registers a LaunchAgent; retires the old menubar agent.

### Fixed (v0.6 — write-back correctness audit)

- **NFS write-back could duplicate or lose data.** `flush_stage` deleted only
  the *original* device object and never updated it, so a second COMMIT
  re-uploaded and orphaned the first copy. It now deletes the *current* device
  copy (`flushed_dev` or `origin_dev`), skips clean stages, and rebinds
  `origin_dev = None` after a successful upload.
- **Deleting a flushed file was a no-op on the device** — the file reappeared
  after remount. `remove` now deletes the device object behind a stage.
- **`create` on an existing name silently truncated it** (empty stage →
  COMMIT wiped the file). Existing objects are now staged with their content;
  an explicit `size` (O_TRUNC) is applied separately.
- **Writes claimed stability they did not have**: `write` echoed the client's
  `stable_how`, so a `FILE_SYNC` request could skip COMMIT and never reach the
  phone. Always reports `UNSTABLE` to force COMMIT.
- New files always flushed to storage index 0; the storage index now comes
  from the parent (internal vs SD card).
- Staged reads returned zero-padded data past EOF (ignored `read` count).
- Directory writes are rejected (`NFS3ERR_ISDIR`) instead of staging a
  directory as a file and deleting it on flush.
- `(size - off) as u32` truncated ≥4 GiB objects during staging.
- `capacity - free` underflowed on device-reported counters.

### Fixed (v0.6 — robustness audit)

- `watch` now waits out an occupied NFS port instead of crash-looping under
  `KeepAlive` (the port doubles as a singleton lock), and re-mounts if the
  volume disappeared underneath it.
- Actor: storage probe is bounded and candidates exposing no storages are
  rejected (USB-UART adapters no longer yield an empty actor); upload
  progress tickers no longer leak when the storage fails to open;
  `DeviceHandle::force_close` aborts a wedged actor and `detach` uses it.
- `test_session` is time-bounded so a hung USB transfer cannot freeze the
  watcher.
- Metadata cache: dead entry index removed, listings pruned and capped.
- `pull_tree` rejects device-reported names containing path separators
  (path-traversal guard).
- `mount`/`unmount`: proper shell quoting (paths with `'` are safe), the
  whole fallback sequence runs in one privileged call (one prompt instead of
  up to nine), and unmount falls back to `-f`.
- Installer adds `ThrottleInterval` to the daemon plist.

### Added (v0.5)

- Menu-bar bridge icon 🌉 (pure Rust + objc2, since replaced in v0.6):
  connection state, model, live transfer rates, open-volume / unmount /
  quit actions. Reads a status JSON published by the watch daemon every
  poll cycle.
- Watch daemon publishes `/tmp/pereprava-status.json` (atomic rename)
  including per-cycle speed computed from adapter traffic counters.
- Traffic counters live in the NFS adapter (rx = device→Mac,
  tx = Mac→device), incremented on ranged reads and stage flushes.


### Added (v0.4)

- Writable Finder volume: write-back staging per ADR-004 — POSIX writes land
  in a local stage and flush to the device on COMMIT as delete+upload;
  `flushed_dev` rebinding keeps kernel filehandles valid across flushes.
- Core: handle-based mutations (HDelete/HUpload/HMkdir/HRename) so the NFS
  adapter never re-resolves paths on the hot path.
- `Error::Disconnected` classification preserved from mtp-rs predicates;
  `connect()` retries 3× against ptpcamerad/post-OTA races; connect_first
  probes every USB candidate instead of trusting enumeration order.
- `mount --read-only`; default mount is writable when the device allows.
- Root-free write E2E harness: `scripts/e2e-write-test.sh`.


### Added (v0.3)

- `pereprava mount` / `unmount`: the phone appears in Finder as a read-only
  NFSv3 volume — no kernel extensions, native `mount_nfs`, one admin prompt.
- `crates/nfs-mount`: fernfs-based adapter (vendored) mapping NFS ids onto
  MTP handles; READ clamped to object bounds (Android rejects over-reads).
- mount options hardened with `soft,retry=1,retrans=2,timeo=50`.
- `--serve-only`, `--export`, `--allow-unprivileged-source-port` debug knobs.

### Added (v0.2)

- `pereprava pack` / `pereprava unpack`: directory tree ⇄ one `.tar.zst`
  MTP object (ADR-003). Measured on Nothing Phone (A065, USB 2.0):
  500 × 8 KiB files 21.95 s raw → **0.09 s bundled (233×)**.
- `pereprava bench --bundle`: raw-vs-bundle comparison phase.
- Benchmarks baseline document: `docs/benchmarks/baseline.md`.

### Deferred by measurement

- ADB zstd transport lane (ADR-003 §2) — bundle-mode removed the
  bottleneck it targeted; revisit on evidence. See baseline doc.

## [0.1.0] — 2026-08-25

### Added

- Workspace scaffold: `pereprava-core`, `pereprava` CLI crate, CI pipeline.
- Safety policy: `forbid(unsafe_code)`, denied `unwrap`/`expect`/`panic`
  lints in production code (ADR-000).
- Architecture decision records 000–003: clean-room/MIT policy, mtp-rs core,
  NFSv3 loopback mounting plan, compression policy gated on benchmarks.
- Core: single-session device actor (info/list/resolve/mkdir_all/remove/
  rename/move_into/download_into/upload_new), TTL metadata cache with
  invalidation, recursive pull/push trees, graceful session close.
- CLI: `ls`, `pull`, `push (--force)`, `mkdir`, `rm -r`, `mv`
  (rename/move semantics), `info`, `doctor` (ptpcamerd/AFT/adb probes),
  `bench` (throughput + integrity via FNV-1a64 roundtrip check).
- Device-gated integration test (`PEREPRAVA_DEVICE=1`) — passing against
  Nothing Phone A065 over USB 2.0 High speed.

### Fixed (discovered on hardware)

- Unclean process exit wedged Android's MTP server → actor Shutdown
  request + close() acknowledgement.
- Android rejects duplicate object names with GeneralError → push checks
  existence, replaces only with `--force`.
- Android rejects no-op cross-parent move to the same handle → `mv` does a
  pure rename when the parent is unchanged.
