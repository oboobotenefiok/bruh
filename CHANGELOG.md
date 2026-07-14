# Changelog

All notable changes to **bruh** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added
- Ongoing improvements, documentation updates, and maintenance.

---

## [0.1.0] - 2026-07-14

**Initial Release** — *Cure Your Terminal Of Amnesia*

bruh is a background daemon + CLI tool that ingests shell history, package installs, git commits, and errors into [Cognee](https://github.com/topoteretes/cognee)'s hybrid graph-vector memory layer, enabling natural language queries across sessions.

### Added

#### Core System
- Async Tokio-based daemon with configurable polling and flushing intervals.
- Unified event schema supporting `ShellCommand`, `PackageInstall`, `GitCommit`, and `PackageManagerProfile` events.
- Offline NDJSON buffer with chunking, corruption detection, persistent retry state, and exponential backoff.
- Persistent cursor system for efficient, truncation-safe file tailing.
- Health monitoring (`health.json`) and manual flush signaling.
- Graceful shutdown handling (SIGTERM/SIGINT) with cleanup.

#### Data Collection
- **Shell history**: Support for bash and zsh (extended history format), multi-line commands, working directory tracking, and regex-based exclusion patterns for secrets.
- **Package managers**: Built-in support for apt, pip, npm, cargo, pkg, brew, winget, and choco with cursor-based diffing.
- **Git integration**: Post-commit hook with Unix socket (fast path) and drop-file fallback; includes hash, message, branch, changed files, and diff summary.
- Command deduplication using SHA-256 and bounded hash sets.

#### Self-Learning Discovery
- LLM cascade (Gemini → Groq → Claude) for unknown package manager profiling.
- Extraction of install/remove verbs, registry/log paths, and confidence scores.
- Local cache (30-day TTL) + Cognee persistence.
- Rate limiting and verbose `--learn` output.

#### Cognee Integration
- Shared HTTP client with connection pooling and `rustls-tls`.
- Structured ingestion via multipart `/api/v1/add`.
- Natural language recall via `/api/v1/recall`.
- Graph enrichment (`cognify`) with proper timeout and retry handling.
- Selective `forget` by date or session.

#### CLI & User Experience
- Natural language shorthand: `bruh "what did I do to fix that error?"`
- Full command suite: `init`, `daemon`, `query`, `explain`, `watch`, `stats`, `improve`, `forget`, `managers`, `providers`, `config`, `buffer`, and `version`.
- Rich terminal output with ANSI colors (NO_COLOR aware), timelines, banners, and status indicators.
- Interactive query mode.
- Configuration via JSON file + environment variables (`BRUH_*`).

#### Platform & Build
- Full support for Linux (x86_64/arm64), macOS (arm64/x86_64), Windows, and Termux (aarch64).
- One-line installers (`install.sh` / `install.ps1`) with prebuilt binaries and source fallback.
- Embedded git hash and reproducible build support.
- Comprehensive tests for parsers, buffer logic, and cross-platform behavior.

#### Security & Reliability
- Secret exclusion patterns and credential masking.
- No file contents or raw environment variables transmitted.
- Detailed [SECURITY.md](SECURITY.md) with scoped vulnerability reporting.

### Changed
- Offline buffer uses NDJSON (instead of SQLite) for zero native dependencies and better portability.
- Hand-rolled CLI parsing to reduce build times on constrained devices (e.g., Termux).
- Discovery uses pure LLM extraction (removed external search) for lower latency and reliability.

### Fixed
- Robust JSON escaping in git hook for special characters and paths.
- Buffer duplication and replay issues during outages.
- Cursor handling for truncated history files.
- Cross-platform path and state consistency.
- Edge cases in shell parsing and package event detection.

### Security
- Prevention of secret leakage via exclusion patterns.
- Pure Rust TLS backend (`rustls-tls`).
- Masked API keys in configuration output.
- Scoped data model (no file contents transmitted).

---

### Upgrade Notes

**For v0.1.0 (Initial Release)**
1. Run `bruh init` to set up your Cognee API key and git hook.
2. Start the daemon: `bruh daemon &`
3. Begin querying: `bruh "what did I install last week?"`

---

[Unreleased]: https://github.com/oboobotenefiok/bruh/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/oboobotenefiok/bruh/releases/tag/v0.1.0
