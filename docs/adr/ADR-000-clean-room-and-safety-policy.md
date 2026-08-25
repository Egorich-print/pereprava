# ADR-000: Clean-room rewrite, MIT license and safety policy

- Status: accepted
- Date: 2026-08-25
- Deciders: Egorich-print

## Context

`pereprava` replaces `simple-mtpfs-mac` (GPLv2, C++17, libmtp + macFUSE kext).
The original is used as a *behavioral reference only* (which Finder-facing
syscalls must exist, known failure modes). No source is read-and-transcribed:
the new implementation is a different language, different architecture and a
different protocol library.

## Decision

1. **Clean-room procedure.** The C++ sources are treated as documentation of
   *what* macOS/Finder requires from an MTP filesystem. Implementation details,
   structure and code are not carried over.
2. **License: MIT** for the whole project. This is defensible because of (1);
   all first-party files carry the standard MIT header via workspace metadata.
3. **Safety policy.**
   - `#![forbid(unsafe_code)]` in every first-party crate.
   - CI denies `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`,
     `clippy::dbg_macro`.
   - Errors are typed with `thiserror`; the CLI boundary reports through
     `anyhow`.
   - Third-party crates may contain their own unsafe internals; they are
     vetted by dependency review (`cargo deny`) rather than forbidden —
     banning them outright would exclude the Rust ecosystem's USB stack.

## Consequences

- GPLv2 obligations do not attach to this codebase (no derivative work).
- Some upstream behaviors must be re-derived by testing against real devices;
  the integration test suite exists for exactly that.
