// This is the  mod.rs. A quick reminder that whether you use the mod.rs naming convention or stay with the name.rs file format is left to you but for this design where there's a lot of submodules and separation of concerns, it is necessary.If you use the mod.rs format like this, your editor may look messy when you have many mod.rs tabs and you don't know which is which...just so you know.Here I declare all the file that I'm expecting to implement for queries. If you look closely, you'll notice that the file names are in line with the queries we listed in the main.rs file.Yeah, every command except version and help. 

// Remember in the main.rs file, we had a print_help() function we called for the catch-all arm of our query? That's your first answer. Secondly, you'll also notice we fetched the version and git hash right in the arm of the version match from the config files from env! macro. That answers your two questions as to where the commands are handled. Now we have just eleven commands to handle here.

pub mod config_cli;
pub mod config;
// Behold the eleven commands, and these are their submodules in this folders being declared.
// Note that we don't list this in main.rs cause it's not a direct child of it.
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

// That's all
pub use config::home_dir;
pub use config::Config;
// Now lets move into the commands one by one. We start with the query as we promised.
