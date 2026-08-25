# Changelog

All notable changes to this project will be documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/).

## [Unreleased]

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
