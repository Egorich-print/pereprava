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

## Mounted volume throughput (v0.6) — the number users feel

The table above measures the raw MTP lane. What a user actually experiences is
the Finder mount, which adds the NFSv3 loopback and the metadata cache on top.
That lane was measured separately, reading a 40 MiB file from the volume three
times (independent files, to avoid any caching effect):

| Configuration | Throughput | vs. before |
|---|---|---|
| before this work | 3.73 MB/s | — |
| drop the pre-read `hinfo` + 2 s attribute cache | 9.9 MB/s | 2.7× |
| `rsize`/`wsize` raised to 1 MiB (ADR: `PREFERRED_IO_SIZE`) | **11.9 MB/s** | **3.2×** |

Three independent runs on the final build: 11.2, 11.9, 11.2 MB/s.

For reference, the CLI reference lane on the same phone and cable sustains
29.0 MB/s (1506 MiB in 54.5 s), so the mount now runs at ~41% of the direct
MTP path. The remaining gap is NFS request framing, not the phone: the
per-chunk round trip is the cost of presenting a block protocol over a
message-oriented transport.

What actually moved the needle, in order:

1. **Not asking for attributes before reading.** Every read issued an `hinfo`
   and then a range read. Dropping the pre-read removed half the protocol
   round trips.
2. **1 MiB NFS I/O size.** The mount negotiated 8 KiB against a 1 MiB
   `fsinfo` transfer preference; the phone's MTP layer is happy with large
   chunks, so the small size was pure framing overhead.

The 2 s attribute cache is deliberately short. It is what makes recursive
walks cheap within one Finder session, but anything longer shows stale sizes
to the user, and the real-parent invalidation (see below) means mutations are
correct immediately — the TTL only bounds what happens when the cache has no
way to know something changed.

## Correctness work measured alongside (v0.6)

A global audit of the four crates found issues that no benchmark would ever
reveal. Each fix below has a regression test; the notable ones:

- **Truncated transfers were reported as success.** A short read or write was
  returned as a completed transfer; `download_into` now compares bytes against
  the declared total, and `upload_into` removes the device object when the byte
  count does not match. Measured consequence: silent partial files, which is
  the worst failure mode for a backup tool.
- **A wedged phone leaked its USB claim.** The storage probe ran in a spawned
  task; on timeout only the receiver was dropped, so the task kept the device —
  and the claim — alive. Every later connection then failed with "device is held
  exclusively by another process" until the USB function was toggled by hand.
  The probe is now aborted, which releases the claim.
- **`close()` reported success when nothing had been closed.** The actor's
  teardown result was discarded, and there was no deadline, so a shutdown
  could hang the daemon forever on an unresponsive phone. It now returns the
  real result, is bounded, and is idempotent.
- **Deletes left the deleted file visible.** Handle-based delete invalidated
  the storage *root* rather than the directory the object was in, so `ls` kept
  reporting a file that was already gone for the length of the cache TTL.
- **Tree transfers followed symlinks.** A link inside a pulled or pushed tree
  dragged an unrelated subtree along; a link to an ancestor walked forever.
  Both sides now use `symlink_metadata`, skip links explicitly, and report how
  many entries were skipped rather than pretending the copy was complete.
- **Bundles could be half-extracted.** Extraction happened directly in the
  destination, so any error left it in an unknown state. It now stages into a
  sibling directory and promotes atomically, and refuses `..` and absolute
  paths in archive entries.

On-device verification after the fixes (Nothing A065, serial `48a859ee`):

| Check | Result |
|---|---|
| `scripts/e2e-write-test.sh` | BYTE-EXACT ✓ |
| `scripts/e2e-overwrite-test.sh` | 3 writes → exactly 1 object, content = last write ✓ |
| 3 MiB write through the Finder volume, read back and compared | BYTE-EXACT ✓ |
| `pack` → phone → `unpack` round-trip | 2 files + 1 dir recovered byte-exact, links reported as skipped ✓ |
| Delete visibility through the mount | gone on the next `ls`, no TTL wait ✓ |
| Malformed path (`/1/./DCIM`) | rejected immediately, without touching the USB session ✓ |

## Reproduce

```shell
pereprava doctor                          # confirm device + USB speed
pereprava bench --size-mib 256 --small-files 500 --bundle
```
