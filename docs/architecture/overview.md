# pereprava — Architecture Overview

> Layer: Execution. Source of truth is this repository; decisions live in
> [`docs/adr/`](../adr/); status in [`STATUS.md`](../../STATUS.md).

pereprava exposes an Android phone to macOS over MTP without kernel
extensions, macFUSE or C bindings. It is a four-crate Rust workspace plus a
Tauri/Svelte front-end.

## Runtime picture

```
                 USB (nusb, pure Rust)
  phone  ───────────────────────────────►  mtp-rs 0.30
                                                 │
                                                 ▼
                                        pereprava-core  (device actor)
                                        one task owns the MTP session;
                                        callers use request/response handles
                                                 │
                        ┌────────────────────────┼───────────────────────┐
                        ▼                        ▼                       ▼
                 pereprava (CLI)         pereprava-nfs              pereprava-widget
                 ls/pull/push/...        fernfs NFSv3 adapter       Tauri tray + Svelte
                 pack/unpack             → mount_nfs (loopback)      (reads status file)
                                         → /Volumes/pereprava
```

The daemon (`pereprava watch`) keeps the NFS listener and the macOS mount
alive while MTP sessions rotate underneath, so the volume survives
disconnect/reconnect without admin prompts (ADR-002, ADR-004).

## Crates

| Crate | Path | Responsibility |
|-------|------|----------------|
| `pereprava-core` | `crates/core/` | MTP device actor, metadata cache, path/name handling (NFD↔NFC), tree ops |
| `pereprava` | `crates/cli/` | CLI commands, `watch` daemon, `mount`/`unmount`, packaging |
| `pereprava-nfs` | `crates/nfs-mount/` | fernfs VFS adapter (file-id scheme, write-back staging), `mount_nfs` automation |
| `pereprava-widget` | `crates/widget/` | Tauri v2 tray + Svelte dashboard (standalone workspace) |

`fernfs` is vendored under `vendor/fernfs` (BSD-3) so it can be patched
locally. The widget is a separate Cargo workspace because `tauri build`
needs the built frontend and must not join `cargo build --workspace`.

## Core: the device actor

MTP is a stateful, serialized protocol on a single USB interface. Rather than
share a `Mutex<Device>`, `pereprava-core` spawns one task that owns the
session and serves typed requests over an `mpsc` channel. Callers get a cheap
`Clone` handle (`DeviceHandle`) whose methods send a request and await a
`oneshot` reply. This makes data races impossible by construction.

Two API surfaces exist:

- **path-based** (`list`, `resolve`, `download`, `upload`, …) — used by the
  CLI; resolves names with case-insensitive, NFC-normalized matching and a
  TTL metadata cache.
- **handle-based** (`hlist`, `hinfo`, `hread_range`, `hdelete`, `hupload`,
  `hmkdir`, `hrename`) — used by the NFS adapter, which already speaks in
  object handles and must not re-resolve paths on the hot path.

`connect_first()` probes every USB candidate and keeps the first that exposes
at least one storage, so unrelated gadgets (USB-UART adapters, PTP cameras)
on the bus cannot shadow the phone. Liveness is checked with a real
`ping()` round-trip — `info()`/`storages()` are cached snapshots.

## NFS adapter and the file-id scheme

The adapter turns MTP into an NFSv3 filesystem. NFS file ids are 64-bit and
pack either a virtual root, a storage root, or a real MTP object:

```
0x1                         device root (lists storages)
0x2 + i                     storage i root
1<<63 | storage<<48 | handle    real object
1<<62 | seq                staged, not yet flushed
```

Android handles fit below 2^48, so the packing is lossless in practice;
`decode` rejects anything else. The scheme is covered by unit tests.

### Write-back staging (ADR-004)

MTP cannot mutate objects in place, so POSIX writes land in a local staging
file and flush on `COMMIT` as delete-then-upload. The stage tracks the
object currently on the device so repeated commits neither duplicate nor
orphan copies; reads prefer the staged copy; directory ids are rejected
(`NFS3ERR_ISDIR`) rather than staged as files.

## Safety and conventions

- First-party crates are `#![forbid(unsafe_code)]`; `clippy::unwrap_used`,
  `expect_used`, `panic` and `dbg_macro` are denied in CI (ADR-000). The
  widget is the sole exception, and only for platform/FFI glue.
- Errors are typed with `thiserror` in `core`/`nfs`; the CLI reports with
  `anyhow`. `mtp-rs` predicates classify disconnect vs. not-found vs.
  wrong-kind.
- `edition 2024`, `rust-version 1.98`.

## External contracts

- **Status file** `/tmp/pereprava-status.json` — written atomically by
  `watch`, consumed by the widget. Small public API; keep it stable.
- **Mount options** `soft,nolocks,vers=3,tcp,rsize/wsize=131072,retry=1,retrans=2,timeo=50`.
- **Environment**: `PEREPRAVA_DEVICE=1` enables the device integration test.
