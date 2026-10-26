<!-- Instructions

This changelog follows <https://keepachangelog.com/en/>.
Categories: added, changed, deprecated, removed, fixed, security.

-->

# Changelog

This crate has not yet been published. Its first release is being prepared as 0.1.0.

## [Unreleased][]

This release has an [MSRV][] of 1.88.

### Added

- Provide `FootfallSink` for VM run, guest-call, host-call and optional instruction observations.
  The adapter uses `no_std + alloc`; applications own runtime/export backends and program contexts.
  Capture-level admission and scope-token correlation preserve explicit incomplete-evidence semantics. ([#199][] by [@waywardmonkeys][])

[Unreleased]: https://github.com/forest-rs/execution/commits/main/
[MSRV]: README.md
[#199]: https://github.com/forest-rs/execution/pull/199
[@waywardmonkeys]: https://github.com/waywardmonkeys
