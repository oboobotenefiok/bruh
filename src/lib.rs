//! Cure your terminal of amnesia.
//!
//! `bruh` is a background daemon and CLI that watches your shell history, package manager
//! events, and git commits, then batches that activity off to [Cognee](https://www.cognee.ai/)'s
//! hybrid graph-vector memory so you can later ask plain-language questions about what you
//! were doing and why. Four operations sit at the center of it: remember (the daemon
//! batching and shipping events as they happen), recall (`bruh <query>`, asking the
//! accumulated memory a question), improve (asking Cognee to re-derive higher-level
//! structure over what's already been ingested), and forget (pruning a session or time
//! range back out).
//!
//! The [`daemon`] module owns the always-on background process, [`cli`] implements every
//! user-facing subcommand, [`discovery`] is the self-learning layer that figures out unknown
//! package managers on the fly, [`cognee`] is the thin client for the memory backend itself,
//! and [`events`] defines the shared event schema all of the above serialize through.
//!
//! This is the library root. Cargo picks this up automatically because it's named lib.rs,
// same way main.rs is picked up as the binary root. Having both means the project compiles
// as a library AND a binary, and the binary just pulls in the library crate. I did this
// mainly so tests/integration.rs (which lives outside src/) can actually reach into these
// modules. Without a lib.rs, integration tests would have no crate to import from.
//
// There used to be a blanket `#![allow(dead_code, unused_imports)]` right here. It's gone
// on purpose now. Blanket-allowing those two lints at the crate root silences them for
// every module, forever, which buries real signal (an orphaned helper, a forgotten wire-up)
// under the same umbrella as genuinely intentional unused code. Anything that truly needs
// to stay unused for now gets a scoped `#[allow(dead_code)]` right on that item with a
// comment explaining why, not a blanket pass for the whole crate.

// Same five modules as main.rs declares, just re-declared here for the library side.
// Yes it feels repetitive to list them twice (once here, once in main.rs) but that's just
// how the binary-plus-library crate pattern works in Rust. Small price to pay.
pub mod cli;
pub mod cognee;
pub mod daemon;
pub mod discovery;
pub mod events;

