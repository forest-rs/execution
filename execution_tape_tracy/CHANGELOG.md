<!-- Instructions

This changelog follows <https://keepachangelog.com/en/>.
Categories: added, changed, deprecated, removed, fixed, security.

-->

# Changelog

This crate has not yet been published. Its first release is being prepared as 0.1.0.

## [Unreleased][]

This release has an [MSRV][] of 1.88.

### Added

- Attach a validated core source map with `ProfilingTraceSink::sources` and inspect attribution
  through `source_location`. Mapped boundaries use authored filenames/lines; unmapped or other
  program instances use explicit bytecode locations. Binding occurs once; snapshot text is not
  transferred to Tracy. Sink types now carry the source binding lifetime. ([#199][] by [@waywardmonkeys][])

- Close active guards inner-to-outer when dropping the sink, including host panic unwinding. ([#199][] by [@waywardmonkeys][])

- Provide `ProfilingTraceSink` and configurable program-symbol labels for Tracy call/host spans,
  keeping Tracy out of the core VM. The crate requires `std` and `tracy-client` 0.19.0. ([#199][] by [@waywardmonkeys][])

### Changed

- Rename the unpublished `execution_tape_profiling` crate to `execution_tape_tracy`.
  Update dependency/import paths; the development executable is now
  `cargo run -p execution_tape_wind_tunnel --bin tracy_capture`. ([#199][] by [@waywardmonkeys][])

[Unreleased]: https://github.com/forest-rs/execution/commits/main/
[MSRV]: README.md
[#199]: https://github.com/forest-rs/execution/pull/199
[@waywardmonkeys]: https://github.com/waywardmonkeys
