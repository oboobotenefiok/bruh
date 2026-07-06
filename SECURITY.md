# Security Policy

## Supported Versions

bruh is currently at v0.1.x. Security updates are applied only to the latest release.

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x:                |

As the project matures and stable releases are tagged, this table will be expanded accordingly.

---

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

If you discover a vulnerability in bruh — especially one involving shell history exfiltration, API key exposure, arbitrary command execution, or Cognee graph poisoning — please report it privately.

### How to Report

Open a [GitHub Security Advisory](https://github.com/oboobotenefiok/bruh/security/advisories/new) on this repository. This keeps the disclosure private until a fix is ready.

If you are unable to use GitHub's advisory system, send a plain-text email describing the issue. Include:

- A clear description of the vulnerability
- Steps to reproduce it
- The version of bruh affected (`bruh version` prints the exact build)
- Your assessment of the potential impact
- Any suggested fix, if you have one

### What to Expect

| Timeline | What happens |
| -------- | ------------ |
| Within 48 hours | You receive acknowledgment that your report was received |
| Within 7 days | You receive an initial assessment — confirmed, needs more info, or not a vulnerability |
| Within 30 days | A fix is developed and a patched release is prepared (critical issues may be faster) |
| After the fix ships | You are credited in the CHANGELOG and release notes, unless you prefer to remain anonymous |

### If the Vulnerability Is Accepted

You will be kept in the loop throughout the fix process. A disclosure date will be coordinated with you before anything is made public. Credit will be given in the release notes.

### If the Vulnerability Is Declined

You will receive a clear explanation of why it was not considered a security issue. If you disagree with the assessment, you are welcome to discuss it further via the same private channel before going public.

---

## Scope

bruh sits in a sensitive position: it runs as a background daemon with read access to your shell history, watches your package manager activity, and transmits structured event data to a remote API (Cognee Cloud). The attack surface is real and taken seriously.

### In scope

- **Shell history exfiltration** — any path by which bruh transmits commands containing secrets (API keys, passwords, tokens) to Cognee or any other remote endpoint, even if those commands matched exclusion patterns that should have blocked them
- **API key exposure** — vulnerabilities that cause `cognee_api_key` or any configured LLM provider key to be logged, transmitted unencrypted, or written to a world-readable file
- **Command injection** — unescaped package names, manager names, or git metadata passed to shell commands that could result in arbitrary code execution
- **Arbitrary file write** — path traversal via package names, git commit messages, or discovered manager registry paths that causes bruh to write outside its designated data directories (`~/.local/share/bruh`, `~/.config/bruh` on Unix; `%LOCALAPPDATA%\bruh`, `%APPDATA%\bruh` on Windows)
- **Unix socket hijacking** — a local attacker replacing the git socket (`~/.local/share/bruh/git.sock`) to intercept or inject events into the daemon's memory graph
- **Cognee graph poisoning** — a crafted package name, commit message, or manager profile that injects malicious content into the Cognee knowledge graph in a way that affects subsequent `recall()` responses
- **Offline buffer tampering** — manipulation of `buffer.ndjson` to cause bruh to replay malicious events to Cognee on reconnection
- **Privilege escalation** — any scenario where bruh's daemon, running as a regular user, can be made to execute code or access files at a higher privilege level
- **Insecure default configuration** — configuration defaults that expose sensitive data or create an exploitable condition without the user having done anything non-standard

### Out of scope

- Bugs that only affect terminal output formatting or colour rendering
- Vulnerabilities in third-party tools that bruh observes but does not control (`apt`, `pip`, `npm`, `cargo`, `winget`, `git`, etc.)
- Issues in Cognee Cloud's own API — report those directly to the Cognee team
- Feature requests or general usability concerns — open a regular GitHub issue for those
- The fact that bruh reads shell history by design — this is the core function of the tool, not a vulnerability

---

## What bruh Stores and Transmits

Being explicit about the data model is part of the security posture.

bruh transmits the following to Cognee Cloud:

- Shell commands (after applying exclusion patterns from `config.excluded_commands`)
- Working directory at the time of each command
- Package install events: manager name, package name, version
- Git commit hashes, messages, branch names, and changed file names
- Discovered package manager profiles (name, install verb, confidence score)

bruh does **not** transmit:

- File contents of any kind
- Environment variables (except as they appear in shell commands, which exclusion patterns should catch)
- Cognee API key or LLM provider keys (these are only sent as `Authorization` headers to their respective services, never stored in Cognee)
- Any data from directories outside the developer's shell history and git repositories

The exclusion pattern list in the default config is designed to catch common secret-exposure patterns (`export.*KEY`, `export.*SECRET`, `export.*TOKEN`, `set.*PASSWORD` on Windows). Users with custom secret-passing patterns should add their own exclusion regexes via `bruh config set excluded_commands`.

---

## Philosophy

bruh runs continuously in the background, reads your shell history, and sends structured data to a cloud API. That is a position of significant trust. A daemon that leaks a secret from your history — even once, even partially — has failed at its most basic obligation.

Security and correctness are treated as the same priority. A bug that causes bruh to transmit an excluded command is not a minor bug. It is a security incident. The exclusion regex system, the `rustls-tls` transport (no system OpenSSL, no certificate store surprises), and the structured event schema (no raw shell output, no file contents) are all deliberate choices made with this trust model in mind.

If you find a way to break that model, please tell us privately. We will fix it, credit you, and be grateful.
