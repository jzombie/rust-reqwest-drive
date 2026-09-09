# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
 (or is loosely based on) Semantic Versioning.

## [0.13.5-alpha] - 2026-09-09

### Changed
- Bump `reqwest` from 0.13.4 to 0.13.5.
- Bump `simd-r-drive` (and `simd-r-drive-entry-handle`) from 0.16.3-alpha to 0.17.1-alpha.
- Bump crate version from 0.13.4-alpha to 0.13.5-alpha to track upstream `reqwest`.
- Refresh compatible dependencies in `Cargo.lock` (no `Cargo.toml` floor changes):
  - `async-trait` 0.1.89 -> 0.1.91 (#37)
  - `tokio` 1.52.3 -> 1.53.0 (#38)
  - `bytes` 1.11.1 -> 1.12.1 (#39)
  - `rand` 0.10.1 -> 0.10.2 (#40)
  - `http` 1.4.1 -> 1.4.2 (#41)
  - `chacha20` 0.10.0 -> 0.10.2 (`cargo deny check`)
  - `h2` 0.4.8 -> 0.4.19 (fixes RUSTSEC-2026-0258, `cargo deny check`)
