// This is the mod.rs for the cli module. Whether you use the mod.rs naming convention or
// keep each submodule's declarations in a file named after itself is really just a matter
// of taste, but for a module with this many submodules it keeps things tidy, the tradeoff
// is your editor can end up with a pile of tabs all labeled "mod.rs" if you're not careful
// about also showing the folder name.
//
// Every file declared below implements one of the commands listed in main.rs's help text,
// with two exceptions: version and help aren't handled here at all. version reads its
// version string and git hash straight out of the env! macro over in main.rs's own match
// arm, and help is just main.rs's print_help() function firing on the catch-all arm. Both
// answers live in main.rs itself, not in a submodule here, so don't go looking for them.
// That leaves the eleven real commands below.

pub mod config;
pub mod config_cli;
pub mod explain;
pub mod forget;
pub mod improve;
pub mod init;
pub mod managers;
pub mod output;
pub mod providers;
pub mod query;
pub mod stats;
pub mod status;
pub mod watch;

pub use config::home_dir;
pub use config::Config;
