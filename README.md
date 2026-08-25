# pereprava

Modern, async, pure-Rust MTP client for macOS. Safe Android ↔ Mac file transfer
with no kernel extensions, no macFUSE and no C bindings.

> «Переправа» (rus.) — a crossing / ferry bridge: gets your files across the cable.

## Why

macOS has no native MTP support, so Android phones never show up in Finder.
The historical fixes were either Google's abandoned *Android File Transfer*,
commercial apps, or macFUSE-based filesystem drivers that require approving a
kernel extension in Recovery mode.

`pereprava` takes a different route:

| Layer | Choice | Consequence |
|---|---|---|
| Protocol | [`mtp-rs`](https://github.com/vdavid/mtp-rs) — pure Rust async MTP/PTP on `nusb` | No libmtp/libusb/FFI; 1–4× faster than libmtp |
| Device access | Single-task **device actor**, request/response handles | MTP-friendly serialization, no data races by construction |
| Mounting (v0.3+) | Local **NFSv3 loopback** server ([fernfs](https://github.com/lunixbochs/fernfs)) → `mount_nfs` | Volume appears in Finder with **zero kexts / system extensions** |
| Speed-ups (v0.2+) | Bundle-mode (`.tar.zst` single object), optional ADB+zstd transport | Attacks MTP's real bottleneck: per-file overhead |

Rewritten from scratch (clean-room) with inspiration from
[simple-mtpfs-mac](https://github.com/su-z/simple-mtpfs-mac); no code shared.
See [docs/adr/ADR-000-clean-room-and-safety-policy.md](docs/adr/ADR-000-clean-room-and-safety-policy.md).

## Status

Early development — see [STATUS.md](STATUS.md) and the
[feature notes](docs/status/). Roadmap lives in STATUS.md too.

## Install (from source)

```shell
git clone https://github.com/Egorich-print/pereprava
cd pereprava
cargo install --path crates/cli
```

Rust 1.98+ (edition 2024) required. No other dependencies on macOS.

## Usage (v0.1)

```shell
pereprava doctor                 # diagnose device access (ptpcamerad, AFT conflicts)
pereprava info                   # device + storage summary
pereprava ls [/some/path]        # list directory on the phone
pereprava pull <remote> [local]  # download file/dir from phone
pereprava push <local> [remote]  # upload file/dir to phone
pereprava mkdir <remote-dir>
pereprava mv <src> <dst>         # rename/move on device
pereprava rm [-r] <remote>       # delete on device
pereprava bench                  # throughput micro-benchmarks
```

## macOS note: ptpcamerad

macOS ships a daemon (`ptpcamerad`) that grabs MTP devices as soon as they are
plugged in. `pereprava doctor` detects this and prints exact remediation steps;
the short version:

```shell
while true; do pkill -9 ptpcamerad 2>/dev/null; sleep 1; done   # while using pereprava
```

## Safety policy

First-party code is `#![forbid(unsafe_code)]`; `clippy::unwrap_used`,
`expect_used`, `panic` are **denied** in CI. Errors are typed
(`thiserror`), the CLI surface returns `anyhow` reports. See ADR-000.

## License

MIT — see [LICENSE](LICENSE).
