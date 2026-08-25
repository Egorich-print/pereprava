# Changelog

All notable changes to this project will be documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/).

## [Unreleased]

### Added

- Workspace scaffold: `pereprava-core`, `pereprava` CLI crate, CI pipeline.
- Safety policy: `forbid(unsafe_code)`, denied `unwrap`/`expect`/`panic`
  lints (ADR-000).
- Architecture decision records 000–003: clean-room/MIT policy, mtp-rs core,
  NFSv3 loopback mounting plan, compression policy gated on benchmarks.
