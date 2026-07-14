//! This build.rs script embeds git commit info and a build timestamp into the binary at
//! compile time. Cargo runs any build.rs automatically before compiling the crate proper,
//! so by the time main.rs compiles, GIT_HASH and BUILD_TIMESTAMP are already available as
//! environment variables baked into the binary via env!(), which is how `bruh --version`
//! can tell you exactly which commit it was built from without needing git installed at
//! runtime. Worth noting, this file has to succeed for the crate to build at all, if it
//! panics, nothing else compiles.

use std::process::Command;

fn main() {
    // Ask git for the short commit hash, something like "a1b2c3d". If we're not in a git
    // repo, or git isn't installed, or anything else goes wrong, we fall back to "unknown"
    // rather than failing the build over a missing hash, a build without git metadata is
    // still a perfectly usable build.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // cargo:rustc-env=KEY=VALUE is the magic string cargo watches for in build script
    // output, it sets an env var that env!("GIT_HASH") can read back at compile time in
    // the rest of the crate.
    println!("cargo:rustc-env=GIT_HASH={}", hash);

    // SOURCE_DATE_EPOCH is a convention some reproducible-build setups use to pin a fixed
    // timestamp so two builds of the same source produce byte-identical output. If it's
    // set, we just record "reproducible" instead of a real timestamp, since the whole
    // point of a reproducible build is that this field shouldn't vary between builds.
    // Otherwise we grab the actual current Unix timestamp.
    let ts = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|e| e.parse::<i64>().ok())
        .map(|_| "reproducible".to_string())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        });
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", ts);

    // By default cargo re-runs build.rs on every build. Telling it to only re-run when
    // .git/HEAD or the refs/heads directory change means we're not needlessly recomputing
    // this on every single `cargo build` when nothing about the commit has actually moved.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
}
