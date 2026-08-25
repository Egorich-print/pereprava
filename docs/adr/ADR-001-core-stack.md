# ADR-001: Core protocol stack = mtp-rs behind a device actor

- Status: accepted
- Date: 2026-08-25

## Context

The original stack was C libmtp + libusb + macFUSE. Pain: FFI safety, global
mutex serializing everything, synchronous blocking calls in FUSE callbacks,
dead partial-object branch causing full-file copies on every open.

## Decision

Build the core on [`mtp-rs` 0.30](https://github.com/vdavid/mtp-rs):

- Pure async Rust on `nusb`; no C dependencies, no pkg-config, no kexts.
- Streaming downloads/uploads with bounded memory (64 KiB USB reads),
  partial reads incl. >4 GB objects, Android quirk handling built-in.
- Measured 1–4× faster than libmtp upstream.

All device access funnels through **one actor task** owning the single
`MtpDevice` session (mtp-rs allows one connection per device). Callers hold a
cloneable handle and send typed requests over an `mpsc` channel, receiving
`oneshot` replies. Concurrency at the *caller* level stays possible for local
work (disk I/O, compression); protocol traffic is serialized inside the actor.

## Alternatives considered

| Alternative | Why not |
|---|---|
| libmtp-rs (FFI wrapper) | keeps the C dependency and its thread model |
| Hand-rolled MTP | mtp-rs already covers quirks we'd have to rediscover |
| go-mtpfs style (Go) | wrong language for the project goals |

## Consequences

- Actor is the single choke point → simple back-pressure story later.
- Write-behind staging and prefetch (phase 2+) live *around* the actor, not
  inside it.
