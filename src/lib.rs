// This is the library root. Cargo picks this up automatically because it's named lib.rs,
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

