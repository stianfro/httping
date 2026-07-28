# Changelog

All notable changes to HTTPing are documented in this file.

## [0.2.0] - 2026-07-28

### Added

- A loopback-only live dashboard with summary cards, a latency chart, and recent probe results.
- Configurable dashboard ports, browser opening, and bounded in-memory history.
- Optional OpenTelemetry metrics over OTLP HTTP/protobuf for command-line and dashboard probes.

## [0.1.0] - 2026-07-28

### Added

- Command-line options for probe count, interval, and timeout.
- DNS validation before probing, with resolved address output.
- Per-probe timestamps, status or error details, and response-header timing.
- Final response and latency statistics.
- Static Linux, macOS, and Windows release archives and installers.

[0.1.0]: https://github.com/stianfro/httping/releases/tag/v0.1.0

[0.2.0]: https://github.com/stianfro/httping/compare/v0.1.0...v0.2.0
