# bruh — Roadmap Memorandum & Implementation Guide

### WeMakeDevs × Cognee Hackathon | June 29 – July 5, 2026

**Author:** oboobotenefiok
**Status:** Pre-build planning document
**Last Updated:** June 28, 2026

---

## 1. Context & Origin

This document captures the full planning process, architectural decisions, and implementation strategy for **bruh** — a background daemon and CLI tool that gives developers a persistent, queryable memory of their entire development activity.

The project was conceived in preparation for the **WeMakeDevs × Cognee Hackathon**, formally titled *"The Hangover Part AI: Where's My Context?"*, running from **June 29 to July 5, 2026**. The hackathon is sponsored by [Cognee](https://github.com/topoteretes/cognee), an open-source, self-hosted, hybrid graph-vector memory layer for AI agents. As you'll expect, there's also special prizes attached.

Before I move forward, I'd love you to know I use emdashes (—) and sometimes sound like AI just in case it irks you.
The only hard technical requirement is that every submission must use Cognee for memory. Everything else — language, stack, theme, platform — is completely open.

---

## 2. The Road to bruh

### 2.1 First Consideration: pkgtrace cognee

The initial idea was to extend **pkgtrace**(https://github.com/oboobotenefiok/pkgtrace), an existing Rust CLI tool built for the Termux ecosystem that scans and manages packages across multiple package managers simultaneously. The proposal was a new subcommand:

```
pkgtrace cognee "How is my memory usage like?"
pkgtrace cognee "What changed since last week?"
pkgtrace cognee "Do I have anything installed in both apt and pip?"
```

The flow was straightforward: `pkgtrace scan` would feed the current package state into Cognee via `remember()`, and `pkgtrace cognee "<query>"` would hit `recall()` to answer natural language questions about package history.

This was evaluated honestly and found to be a **solid but narrow idea**. It solves a well-defined problem, uses Cognee properly, and is buildable in 7 days. However, the scope is limited to packages. Developers already have workarounds for this — `history | grep`, scripts, README notes. It's a "nice to have", not a "how did I live without this."

There was also a compliance question: would submitting a Cognee-powered version of an existing project violate the hackathon rules? The rules were fetched and reviewed directly. They do not explicitly ban existing projects — Rule 7 states that templates, frameworks, libraries, and public APIs are all permitted, and that "original work built on top of these will be judged." Rule 9 only restricts coding and design work from starting before June 29. The safe path, however, is to start a fresh repository on June 29 and write new code during the hackathon window, using pkgtrace's architecture as prior knowledge rather than copied source.

### 2.2 The Pivot: bruh

The better idea emerged from thinking about what Cognee's graph-vector hybrid actually enables that plain vector search does not. The fundamental insight is this:

> **Developer context is causal, not just semantic.** 

— Now, read that again!

Let me tell you somethimg, when a developer asks "what did I do to fix that build error last month?", they do not want the document most similar to the phrase "fix build error" like conventional searches would do. They want the actual sequence of events: the error output, the command that preceded it, the package that was installed, the file that was changed, and the commit message that closed the loop. That is a **graph traversal**, not a vector similarity match. This is the meat of this project!!!!!!!

Now, wait!!! Terminal history is flat. Git logs are siloed. Package manager logs are scattered especially at dependency level which I was trying to solve with pkgtrace (https://github.com/oboobotenefiok/pkgtrace). Nothing connects them into a unified, queryable memory.

But Guess What?

bruh connects them.

### 2.3 The DevTrace Digression

A variant proposal emerged during planning under the name **DevTrace**, which attempted to merge pkgtrace's package scanning scope with bruh's broader activity memory. It added file watching and granular query flags (`--packages`, `--files`, `--errors`, `--stats`).

After honest evaluation, DevTrace was found to be bruh with a new name and added scope. The file watcher is the only genuinely new architectural element, and it is non-trivial to implement correctly in Rust on Linux (inotify) and macOS (kqueue) within a 7-day window. The granular CLI flags are good UX but do not change the architecture.

The verdict: I resolved to keep the name **bruh**, keep the scope tight, optionally add the file watcher only if the first four days go cleanly ahead of schedule. The `--stats` productivity output is worth including regardless — it is low implementation cost and extremely high demo value.

---

## 3. The Problem bruh Solves

Every developer has lived this:

- You fixed a cryptic build error three weeks ago. You do not remember what you did.
- You installed a package and have no idea why it is in your system.
- You spent 45 minutes debugging something you have already debugged before.
- You return to a project after a break and spend the first hour just reconstructing context.

The root cause is that development environments are **stateless between sessions**. Shell history is a flat list with no context. Git logs capture commits but not the struggle that led to them. Package managers record installs but not the error that triggered them. There is no layer that connects these events into a coherent, persistent narrative.

But Guess What?

bruh is that layer.

---

## 4. What bruh Does

bruh runs two components:

**A background daemon** that continuously watches your development activity and ingests it into Cognee's memory layer. It watches:

- Shell command history (zsh, bash)
- Package manager events (apt, pip, npm, cargo)
- Git commits and messages
- stderr output from commands
- *(Stretch goal)* File system changes in project directories

**It's a CLI interface** that lets you query that memory in natural language at any time, from any session, with no context window limitations.

Take a look at these:
```bash
bruh "What did I do to fix that segfault last month?"
bruh "When did I install libssl and why?"
bruh "What changed right before my build broke?"
bruh "What's my most repeated error this week?"
bruh --stats
bruh forget --before "last week"
bruh forget --session "project-x"
```

The output is not a chatbot response of 1 + 1 = 2 like you may expect but a **structured timeline** with timestamps, commands, errors, and causal connections — reconstructed from Cognee's graph traversal.

```
$ bruh "what did I do to fix that error"
→ Found memory session (2 hours ago)
  14:32:18  npm install sqlite3          [exit 1]
  14:32:45  error: "Can't find Python"
  14:33:12  brew install python@3.11     [exit 0]
  14:33:45  npm install sqlite3          [exit 0]
  14:34:02  git commit -m "fix: add python dep for sqlite3"
```

Developers have all lived this exact scenario, I know you can feel it.

---

## 5. Why Cognee Specifically


Cognee is a **hybrid graph-vector memory layer**. The graph layer models relationships between entities. For bruh, those relationships are somethimg like this:

```
[Command] ──has_output──> [Error]
    │                         │
    │                         └──led_to──> [Fix Command]
    │
    └──preceded──> [Command]

[Fix Command] ──installed──> [Package]
[Package] ──required_by──> [Project]
[Git Commit] ──fixes──> [Error]
```

When you query "Why did I install openssl?", Cognee does not find the word "openssl" in a vector space. It traverses the graph: finds the install command, follows the `preceded_by` edge to the failed build command, reads the error output, and returns the full causal chain. This is fundamentally more powerful than vector search for temporal, causal developer data.

bruh exploits this fully. It is one of the few concepts where the graph layer is load-bearing, not decorative.

---

## 6. Cognee API Usage

bruh uses all four of Cognee's core memory operations meaningfully:

**`remember()`** — Called by the daemon on every ingested event. Each event is serialized as a structured text blob with metadata (timestamp, event type, exit code, working directory) and posted to Cognee. This builds the persistent knowledge graph over time. As at the time of writing this, I've not yet confirmed if I can send json to cognee...I know they split date across various database types but that is not something I have to worry about much... I'll figure it out when I'm done going through the docs.

**`recall()`** — Called by the CLI on every user query. The natural language query is posted to Cognee, which routes it between semantic vector search and graph traversal automatically. The response is parsed and formatted into a timeline for the terminal.

**`improve()` / memify** — Run periodically (or on-demand via `bruh improve` command) to enrich the memory graph. This clusters recurring errors, identifies repeated fix patterns, prunes redundant nodes, and surfaces statistics like most common error types and average fix times.

**`forget()`** — Exposed via `bruh forget` with flags for time ranges and session labels. Lets users surgically prune old or irrelevant memory without destroying the entire graph.

---

## 7. Architecture

### 7.1 High-Level

```
┌─────────────────────────────────────┐
│           bruh daemon            │
│                                     │
│  shell.rs   ──> shell history       │
│  packages.rs──> apt/pip/npm/cargo   │
│  git.rs     ──> commit events       │
│  stderr.rs  ──> error output        │
│                                     │
│  Batches events every 30-60 seconds │
└──────────────┬──────────────────────┘
               │ reqwest (async HTTP)
               ▼
┌─────────────────────────────────────┐
│         Cognee Cloud REST API       │
│                                     │
│  POST /remember  ← daemon           │
│  POST /recall    ← CLI queries      │
│  POST /improve   ← periodic/manual  │
│  POST /forget    ← manual pruning   │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│           bruh CLI               │
│                                     │
│  bruh "<query>"                  │
│  bruh --stats                    │
│  bruh forget --before "..."      │
│  bruh improve  
│ <I'll add other commands as it grows>
└─────────────────────────────────────┘
```

### 7.2 Daemon Design

The daemon is a long-running Tokio async process. It does not use inotify or kqueue for shell history — instead, it polls `~/.zsh_history` and `~/.bash_history` on a configurable interval (default 30 seconds), diffs against its last known state, and ingests new entries. This is simpler and more portable than filesystem event watchers.

For package managers, the daemon listens for events differently per manager:

- **apt/dpkg:** tail `/var/log/dpkg.log`
- **pip:** hook via `pip`'s post-install (or poll `pip list` and diff)
- **npm:** tail `~/.npm/_logs/`
- **cargo:** watch `~/.cargo/registry/` for new entries

For git, a `post-commit` hook is installed at setup time (`bruh init`) that fires a local socket message to the daemon with the commit hash, message, and diff summary.

All events are queued in memory and flushed to Cognee in batches every 30-60 seconds. If Cognee Cloud is unreachable, events are written to a local SQLite buffer and retried on the next cycle.
If the API_KEYS aren't accessible, it throws an error.

### 7.3 Event Schema

Every event sent to Cognee's `remember()` is serialized as structured text in the following format:


But wait, I'll be building this project purely in Rust.

```
EVENT: shell_command
TIMESTAMP: 2026-06-30T14:32:18Z
DIRECTORY: /home/obot/projects/myapp
COMMAND: npm install sqlite3
EXIT_CODE: 1
OUTPUT: npm ERR! gyp ERR! find Python
DURATION_MS: 4821
```

```
EVENT: package_install
TIMESTAMP: 2026-06-30T14:33:12Z
MANAGER: brew
PACKAGE: python@3.11
VERSION: 3.11.9
TRIGGER_COMMAND: npm install sqlite3
EXIT_CODE_TRIGGER: 1
```

```
EVENT: git_commit
TIMESTAMP: 2026-06-30T14:34:02Z
HASH: a3f92c1
MESSAGE: fix: add python dep for sqlite3
FILES_CHANGED: package.json, README.md
BRANCH: main
```

Structured text (rather than JSON) is used because Cognee's `remember()` ingests natural language and document formats — the structured fields give the graph builder enough signal to create meaningful edges without requiring a custom schema. READ THAT AGAIN!!!!

### 7.4 CLI Design

The CLI is built with `clap` and communicates with Cognee Cloud via `reqwest`. Queries are sent to `recall()` and the response is parsed and pretty-printed to the terminal.

```rust
// CLI entry points
bruh "<query>"              // natural language recall
bruh --stats                // improve() → summarized patterns
bruh improve                // manual memify trigger
bruh forget --before <date> // time-based pruning
bruh forget --session <id>  // session-based pruning
bruh init                   // setup: install git hook, configure API key
bruh daemon                 // start the background daemon
```

Output formatting prioritizes terminal readability: timestamps in a muted color, commands in bold, errors in red, success in green. No external TUI crates — plain ANSI escape codes to keep the binary lean. Hope I remember to do this.

---

## 8. Project Structure


Should look something like this:

```
bruh/
├── src/
│   ├── main.rs              # CLI entry point, clap routing
│   ├── daemon/
│   │   ├── mod.rs           # Daemon loop, event queue, batch flush
│   │   ├── shell.rs         # zsh/bash history polling and diffing
│   │   ├── packages.rs      # apt/pip/npm/cargo event ingestion
│   │   ├── git.rs           # git hook listener via local socket
│   │   └── buffer.rs        # SQLite offline event buffer
│   ├── cognee/
│   │   ├── mod.rs           # Cognee API client (reqwest)
│   │   ├── ingest.rs        # remember() — event serialization + POST
│   │   ├── query.rs         # recall() — query POST + response parsing
│   │   ├── improve.rs       # improve()/memify trigger
│   │   └── forget.rs        # forget() with time/session filters
│   ├── cli/
│   │   ├── output.rs        # Terminal formatting, ANSI colors, timelines
│   │   └── config.rs        # API key management, daemon config
│   └── events/
│       └── schema.rs        # Event types, serialization
├── hooks/
│   └── post-commit          # Git hook script installed by bruh init
├── examples/
│   └── demo.sh              # Reproducible demo script for judges
├── README.md
└── Cargo.toml
```

---

## 9. 7-Day Execution Plan

### Day 1 — Foundation

- Set up fresh GitHub repository (`bruh`)
- Scaffold the full project structure per Section 8
- Implement the Cognee REST API client in `cognee/mod.rs`
- Wire a hardcoded test event through `remember()` and verify it reaches Cognee Cloud
- Implement `recall()` with a hardcoded query and verify the response
- Goal: end of day 1, `remember()` and `recall()` work with hardcoded data

### Day 2 — Shell History Ingestion

- Implement `daemon/shell.rs`: poll `~/.zsh_history` and `~/.bash_history` on interval
- Implement state diffing to extract only new commands since last poll
- Serialize commands into the event schema from Section 7.3
- Wire through `ingest.rs` to `remember()`
- Test: run a few commands, watch them appear in Cognee

### Day 3 — Package Manager + Git Ingestion

- Implement `daemon/packages.rs`: apt dpkg.log tailing, pip polling, npm log tailing, cargo registry watching
- Implement `daemon/git.rs`: install post-commit hook via `bruh init`, listen on local socket
- Serialize package events and commits into schema
- Wire all through ingestion pipeline
- Implement `daemon/buffer.rs`: SQLite offline queue with retry logic

### Day 4 — CLI recall() Output

- Implement full `bruh "<query>"` flow
- Parse Cognee's `recall()` response into structured timeline
- Implement `cli/output.rs`: ANSI-colored terminal formatting
- Test with real queries against real ingested data
- Goal: end of day 4, the core demo loop works end-to-end

### Day 5 — improve() and forget()

- Implement `bruh improve`: trigger Cognee's memify, print summary of patterns found
- Implement `bruh --stats`: surface most common errors, average fix times
- Implement `bruh forget --before <date>` and `bruh forget --session <id>`
- Wire `cli/config.rs`: API key stbruhge, daemon config file at `~/.config/bruh/config.toml`

### Day 6 — Polish and Demo

- Write `examples/demo.sh`: a fully reproducible 2-minute demo script
- Pre-record the demo session (see Section 10)
- Error handling pass: every network failure, missing config, and malformed response handled gracefully
- Install script: `curl | sh` one-liner that installs the binary and runs `bruh init`
- README first draft

### Day 7 — Submit

- Final README: problem statement, demo GIF or video link, install instructions, architecture diagram
- Cut any unfinished scope ruthlessly — a polished core beats a broken full feature set
- Submit before deadline

---

## 10. The Demo

The demo is the most important artifact after the code itself. Judges are busy. The demo must communicate the value of bruh in under two minutes.

**The script:**

1. Open a terminal. Run `bruh daemon` in a background pane.
2. Simulate a real bug encounter: `cargo build` → error about missing openssl → `sudo apt install libssl-dev` → `cargo build` → success → `git commit -m "fix: add libssl-dev dependency"`
3. Wait 30 - 60 seconds (daemon flush interval).
4. In a new terminal session (critical — this proves cross-session memory), run:

```bash
$ bruh "What did I do to fix that build error?"
```

5. Show the output: the full causal timeline, timestamped, with the error, the fix command, and the commit.
6. Run `bruh --stats` to show the productivity summary.


---

## 11. Judging Criteria Alignment

The hackathon judges on six criteria. Here is how bruh addresses each:

**Potential Impact:** Every developer on the planet has lost time retracing their steps. bruh solves a universal, daily pain point. The market is every developer with a terminal.

**Creativity & Innovation:** No tool currently connects shell history, package events, git commits, and error outputs into a unified queryable graph. This is a genuinely new category of developer tooling.

**Technical Excellence:** Rust daemon with async Tokio runtime, structured event schema, SQLite offline buffer, clean clap CLI, proper error handling throughout. The codebase will be readable and maintainable.

**Best Use of Cognee:** All four memory lifecycle operations are used meaningfully, not decoratively. The graph traversal is load-bearing — the causal chain reconstruction is impossible without it. This is one of the few concepts where the graph layer does real work.

**User Experience:** The CLI is simple, the output is human-readable, the install is a one-liner. There are no configuration files to write before first use -- except API key. `bruh init` handles everything.

**Presentation Quality:** The README will include a problem statement, a demo GIF, a clear architecture diagram, and a one-liner install. The demo script is reproducible by anyone with a Linux or macOS terminal.

---

## 12. Risk Register

| Risk | Likelihood | Mitigation |
|---|---|---|
| Cognee Cloud API latency too high for real-time feel | Medium | Batch ingestion every 30-60s. Daemon is async, CLI is synchronous query — latency only matters at query time, which is acceptable. |
| Shell history parsing breaks on edge cases | Medium | Focus on standard zsh/bash EXTENDED_HISTORY format. Skip malformed lines silently. |
| Git hook installation fails on some systems | Low | Fallback: poll `git log` on interval instead of hook. Less elegant but functional. |
| Scope creep kills the deadline | High | File watcher is explicitly cut from MVP. Stats and `improve()` are cut on Day 5 if behind. Core is `remember()` + `recall()` working cleanly. |
| Cognee Cloud goes down during submission week | Low | Keep the local Cognee self-hosted option documented as a fallback in the README. |
| Privacy concerns from reviewers | Low | Document clearly in README: only structured event metadata is sent, not file contents. User controls what is ingested via config. |

---

## 13. Rules Compliance

The hackathon rules were reviewed in full. bruh is compliant on every point:

- **Rule 2 (Required tech):** Cognee is the memory layer. All four lifecycle ops are used.
- **Rule 7 (Existing work):** bruh is a fresh repository started June 29. pkgtrace and prior Rust experience are prior knowledge, not submitted code.

- **Rule 9 (No pre-build coding):** No code is written before June 29. This document is planning only — notes, diagrams, and architecture sketches are explicitly permitted.

---

## 14. What We Are Not Building

To be explicit about scope boundaries:

- **Not a web app.** The submission is a terminal tool. This is intentional — it stands out from the field and is the correct UX for the target user.
- **Not a file content indexer.** bruh does not read or store the contents of source files. It tracks commands, errors, packages, and commit messages only.
- **Not a real-time streaming system.** The daemon batches and flushes every 30-60 seconds. This is sufficient for the use case and avoids API rate limit issues.
- **Not a pkgtrace fork.** bruh is a new project. pkgtrace's domain knowledge informed the package manager ingestion module, but no code is shared.

---

## 15. Final Statement

The submission's sponsored technology is not a wrapper but the actual foundation. Cognee's graph-vector hybrid is not decorative here — it is what makes the causal reconstruction possible.

The demo is immediate and visceral.

Fresh repo. June 29.

---
