# ADR-003: Compression policy — measured or cut

- Status: accepted
- Date: 2026-08-25

## Context

USB 2.0 (~40 MB/s ceiling) invites "just compress it" ideas. But:

- Transparent per-file compression **inside MTP is impossible**: the phone
  controls its storage; USB bulk transfers carry raw object bytes.
- Photos/videos dominate real transfer profiles and are already compressed;
  zstd cannot shrink them.
- MTP's actual killer for small files is per-object protocol overhead, which
  compression does not address directly — bundling does.

## Decision

Two mechanisms, both gated by measurement:

1. **Bundle-mode** (MTP-compatible): pack a directory tree into one
   `.tar.zst` stream uploaded as a single object. Kills per-file overhead N+1
   round-trips → 1. Unpack happens phone-side via ADB when available, or the
   archive is simply kept/pulled back as one file.
2. **ADB transport** (optional, auto-detected): if `adb devices` shows an
   authorized device, offer a tar+zstd pipe mode with true wire compression.
   Never required; plain MTP always works without developer mode.

**Gate:** a mechanism ships only if `pereprava bench` shows ≥25% improvement
on the Nothing Phone (2) baseline profile (1 GB video, 1000-file doc tree).
Otherwise the idea is documented as "tried, not profitable" in
`docs/benchmarks/` and dropped. Entropy sampling skips incompressible payloads
automatically.
