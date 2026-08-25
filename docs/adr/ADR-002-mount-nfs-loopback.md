# ADR-002: Finder mounting via NFSv3 loopback server

- Status: accepted (implementation deferred to v0.3)
- Date: 2026-08-25

## Context

macOS offers four ways to surface a user-space filesystem:

| Option | Kext? | Verdict |
|---|---|---|
| macFUSE (+ fuser) | kernel extension, Recovery-mode approval on Apple Silicon | rejected: adoption blocker, deprecated direction |
| fuse-t | no | rejected: no Rust FUSE crate supports it (fuser/fuse3/rfuse3 hardcode macFUSE) |
| FSKit (macOS 15.4+) | no | deferred: app-extension packaging + Apple-only entitlement, immature for OSS CLI |
| **NFSv3 loopback** | **no** | chosen |

Precedents: Facebook EdenFS (macOS kernel integration via own NFSv3 server),
anylinuxfs, Cryptomator's WebDAV volume, fuse-t internally.

## Decision

Implement mounting as a local NFSv3 server (crate `fernfs`, vendored into the
workspace) exposing the MTP VFS, then mount it with macOS's native
`mount_nfs` on `127.0.0.1`. The volume appears in Finder as a network drive.

Mount requires admin rights once per mount (osascript prompt in v0.3 MVP).

Constraints we accept: network-volume semantics (no Spotlight by default, no
kqueue events), loopback bind only, privileged source-port checks stay on.

## Phasing guard-rail (project review feedback)

NFS work starts **only after** CLI/core prove stable (v0.1 shipped and dog-
fooded) and benchmarks are published (v0.1.x). Not earlier.
