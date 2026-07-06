//! TEST-006: integration tests for the event pipeline.
//! Uses the real parsing code with fixture files.
// This file lives outside src/ (in tests/) which is how Cargo knows to treat it as an
// integration test crate rather than a unit test module, it compiles against `bruh` as an
// external dependency, exactly the way a real consumer of the library would, using only
// the public API (that's why lib.rs exists at all, see the comment there). The focus here
// is the stuff that's genuinely worth testing end-to-end: serde round-tripping for every
// Event variant, since a schema mismatch there would silently corrupt data going to or
// coming from the offline buffer, plus the hashing and classification helpers that a lot
// of other logic depends on being correct.

use std::io::Write;
use tempfile::NamedTempFile;

// ── Shell history parser integration ─────────────────────────────────────────

// Not actually used by any test below right now, honestly. I built it while I was testing
// the shell parser against real fixture files early on and it's stuck around since it's
// cheap to keep and might be handy again if I add a fixture-based parser test later.
fn zsh_fixture(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    write!(f, "{}", content).unwrap();
    f
}

/// Directly test the public-facing shell module logic via the events schema.
#[test]
fn test_command_hash_normalises_whitespace() {
    use bruh::events::command_hash;
    let h1 = command_hash("cargo  build  --release");
    let h2 = command_hash("cargo build --release");
    let h3 = command_hash("  cargo build --release  ");
    assert_eq!(h1, h2);
    assert_eq!(h2, h3);
}

#[test]
fn test_command_hash_differs_for_different_commands() {
    use bruh::events::command_hash;
    let h1 = command_hash("cargo build");
    let h2 = command_hash("cargo test");
    assert_ne!(h1, h2);
}

#[test]
fn test_classify_error_variants() {
    use bruh::events::classify_error;
    assert_eq!(classify_error("linker 'cc' not found"), Some("linker_error".into()));
    assert_eq!(classify_error("permission denied"), Some("permission_denied".into()));
    assert_eq!(classify_error("cannot find -lssl"), Some("missing_dependency".into()));
    assert_eq!(classify_error("error[E0499]: cannot borrow"), Some("compile_error".into()));
    assert_eq!(classify_error(""), None);
}

// ── NDJSON buffer integration ─────────────────────────────────────────────────
// These three round-trip tests each serialize an Event to JSON and deserialize it straight
// back, checking the fields survive the trip intact. This matters way more than it might
// look like at first glance, this exact serialize/deserialize path is what buffer.rs relies
// on to persist events to disk during a Cognee outage and read them back later, so a subtle
// serde bug here would mean silently losing or corrupting data exactly when the offline
// buffer is needed most.

#[test]
fn test_ndjson_round_trip() {
    use bruh::events::{Event, ShellCommandEvent};
    use chrono::Utc;

    let event = Event::ShellCommand(ShellCommandEvent {
        timestamp:    Utc::now(),
        directory:    "/tmp/test".into(),
        command:      "cargo build".into(),
        exit_code:    Some(0),
        output:       None,
        duration_ms:  Some(1234),
        session_id:   Some("session_123".into()),
        command_hash: Some("abc123".into()),
        error_type:   None,
    });

    let json = serde_json::to_string(&event).unwrap();
    let restored: Event = serde_json::from_str(&json).unwrap();

    match restored {
        Event::ShellCommand(e) => {
            assert_eq!(e.command, "cargo build");
            assert_eq!(e.session_id, Some("session_123".into()));
            assert_eq!(e.exit_code, Some(0));
        }
        _ => panic!("Wrong event variant"),
    }
}

#[test]
fn test_package_install_event_serde() {
    use bruh::events::{Event, ManagerType, PackageInstallEvent};
    use chrono::Utc;

    let event = Event::PackageInstall(PackageInstallEvent {
        timestamp:         Utc::now(),
        manager:           "apt".into(),
        manager_type:      ManagerType::Bootstrapped,
        package:           "libssl-dev".into(),
        version:           Some("1.0.2".into()),
        trigger_command:   Some("cargo build".into()),
        exit_code_trigger: Some(1),
        session_id:        Some("session_456".into()),
        working_directory: Some("/home/user/project".into()),
    });

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("package_install"));
    assert!(json.contains("libssl-dev"));

    let restored: Event = serde_json::from_str(&json).unwrap();
    match restored {
        Event::PackageInstall(e) => {
            assert_eq!(e.package, "libssl-dev");
            assert_eq!(e.trigger_command, Some("cargo build".into()));
            assert_eq!(e.working_directory, Some("/home/user/project".into()));
        }
        _ => panic!("Wrong variant"),
    }
}

#[test]
fn test_git_commit_event_serde() {
    use bruh::events::{Event, GitCommitEvent};
    use chrono::Utc;

    let event = Event::GitCommit(GitCommitEvent {
        timestamp:         Utc::now(),
        hash:              "abc1234".into(),
        message:           "fix: add libssl-dev".into(),
        branch:            "main".into(),
        files_changed:     vec!["Dockerfile".into()],
        session_id:        Some("session_789".into()),
        working_directory: Some("/home/user/project".into()),
        diff_summary:      Some("1 file changed, +1".into()),
    });

    let json = serde_json::to_string(&event).unwrap();
    let restored: Event = serde_json::from_str(&json).unwrap();
    match restored {
        Event::GitCommit(e) => {
            assert_eq!(e.hash, "abc1234");
            assert_eq!(e.diff_summary, Some("1 file changed, +1".into()));
            assert_eq!(e.working_directory, Some("/home/user/project".into()));
        }
        _ => panic!("Wrong variant"),
    }
}

#[test]
fn test_corrupt_ndjson_skipped() {
    // Simulates BUFFER-003: corrupt lines should be skippable
    let lines = vec![
        r#"{"event_type":"shell_command","timestamp":"2024-01-01T00:00:00Z","directory":"/","command":"ls","exit_code":0,"session_id":null,"command_hash":null,"error_type":null,"output":null,"duration_ms":null}"#,
        "this is not json at all",
        r#"{"event_type":"shell_command","timestamp":"2024-01-01T00:00:01Z","directory":"/","command":"pwd","exit_code":0,"session_id":null,"command_hash":null,"error_type":null,"output":null,"duration_ms":null}"#,
    ];

    let valid: Vec<_> = lines.iter()
        .filter_map(|l| serde_json::from_str::<bruh::events::Event>(l).ok())
        .collect();

    // Corrupt line is skipped, 2 valid events pass through
    assert_eq!(valid.len(), 2);
}
