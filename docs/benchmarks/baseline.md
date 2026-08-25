# Benchmarks — Nothing Phone (A065), USB 2.0 High speed

Environment: MacBook (Apple Silicon, Darwin), cable USB-C↔USB-C,
Nothing Phone model A065 ("CAPE-QRD"), Android MTP via mtp-rs 0.30.
`pereprava bench` measures end-to-end including local staging I/O.
Payloads are deterministic xorshift data (incompressible by design) unless noted.

## Baseline (v0.1.0 raw MTP)

| Phase | Size | Result |
|---|---|---|
| big push | 64 MiB | 35.77 MiB/s |
| big pull | 64 MiB | 36.91 MiB/s |
| big push | 256 MiB | 37.35 MiB/s |
| big pull | 256 MiB | 36.67 MiB/s |
| roundtrip checksum (FNV-1a64, 256 MiB) | ok | `0x5c07…e2ba` |
| small push | 500 × 8 KiB | 34.96 ms/file → **17.48 s** |
| small push | 200 × 8 KiB | 39.25 ms/file → **7.85 s** |
| readdir | 500 entries | 573 ms |
| readdir | 200 entries | 251 ms |

Reading: sequential transfers sit at ~37 MiB/s ≈ the practical ceiling of
USB 2.0 High speed (~60 MiB/s theoretical). The dominant cost for many-file
workloads is per-object protocol overhead (~35–44 ms/object), not bandwidth.

## Bundle-mode (v0.2) — ADR-003 gate decision data

Same 500 × 8 KiB tree pushed as ONE `.tar.zst` object:

| Mode | Wall time | Throughput equivalent |
|---|---|---|
| raw (500 objects) | 21.95 s | ~0.18 MiB/s effective |
| bundle (1 object, 3.9 MiB → 12 KiB) | **0.09 s** | ~43 MiB/s |
| **Speedup** | **233.6×** | |

Gate from ADR-003 was "≥25% improvement or cut". Result: +23 000%.
Bundle-mode ships.

## Compression honesty note

The 8 KiB test payload compresses extremely well (xorshift with a fixed
seed is trivially predictable). For already-compressed media expect ratio
≈ 1.0× — bundle-mode still wins there purely by removing per-object
overhead; zstd just becomes neutral. `pereprava pack` prints the achieved
ratio so users can see what actually happened to their data.

## ADB lane decision

The optional ADB+zstd transport (ADR-003 §2) is **deferred**: after
bundle-mode the measured bottleneck it addresses (wire bytes for
compressible payloads) no longer dominates any realistic profile we test,
and keeping v0.2 scope tight was an explicit review requirement. Revisit if
a workload appears where MTP metadata latency itself (not transfer volume)
is the blocker AND adb is guaranteed present.

## Reproduce

```shell
pereprava doctor                          # confirm device + USB speed
pereprava bench --size-mib 256 --small-files 500 --bundle
```
