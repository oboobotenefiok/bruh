// Thanks for opting to read through my code base

// THIS CODEBASE HAS VERY VERBOSE COMMENTS AS IT IS TAILORED FOR LEARNING PURPOSES

// I'll be explaining every line of code contextually while building.
// I hope this serves as a great learning point for you.

// Given the warnings I will surely have through the code, I actually don't want to see anything other than actual red errors cause we're in a hackathon and time is never friendly.
#![allow(dead_code, unused_imports, unused_variables)]

// Based on my project plan, these are the modules I'll primarily need for everything to be complete. I declare them using mod. If anything else comes up as I iterate, I'll add them here.
mod cli;
mod cognee;
mod daemon;
mod discovery;
mod events;

// At first I wanted using thiserror crate to experiment but I've come to realise I haven't used it much before now and there's no need taking such risk. I'll stay with anyhow for now.
use anyhow::{anyhow, Result};
use log::info;

// This is the part I have a lot to talk about. First of all, there's a crate called clap, that can handle commands for us quite neatly; I've built several projects with it but this one? I refuse to use it for this project because I'm operating from a quite inferior device. It usually takes long for heavy dependencies to be compiled and sometimes I experience crashes due to RAM shortages or so. To avoid meeting that issue towards the deadline, I'll have to handroll the Parser manually in the code. I'll also do that to several other crates that would otherwise make the project a bloatware. I know this doesn't matter to the final executable as Rustc will do all the optimizations but it's the best approach now for my convenience.

// This is a list of planned commands. I'll do some sort of matching later on but I keep it here as a reference to an array of string slices I can pull later on. It'll be a global constant.
const KNOWN_CMDS: &[&str] = &[
    "init",
    "daemon",
    "query",
    "stats",
    "forget",
    "improve",
    "managers",
    "providers",
    "config",
    "explain",
    "watch",
    "version",
    "--version",
    "-v",
    "--help",
    "-h",
];

// Rust functions are sync by default. To activate async, we apply the tokio::main 'attribute.'
#[tokio::main]
async fn main() {
    // PALETTE-001: left to its own devices, `-> Result<()>` on main() prints failures via
    // Rust's default Termination impl, which uses anyhow's Debug output ("Error: <chain>"),
    // plain and uncolored, that's exactly the flat text the earlier /recall timeout showed
    // up as. Wrapping the real body in run() and handling the Err case ourselves here means
    // every single error path in bruh, no matter how deep it propagates from, surfaces
    // through the same orange, human-readable formatting instead of Rust's raw default.
    if let Err(e) = run().await {
        eprintln!("{} {}", cli::output::orange("Error:"), e);
        // anyhow's error chain: each `.context()`/`with_context()` call up the stack adds
        // one link here, so a network failure three modules deep still shows its full story
        // instead of just the outermost, least specific message.
        for cause in e.chain().skip(1) {
            eprintln!("  {} {}", cli::output::dim("caused by:"), cause);
        }
        std::process::exit(1);
    }
}

// The actual program body, moved out of main() so the wrapper above can catch whatever
// error comes back and print it consistently instead of leaving that to Rust's default.
async fn run() -> Result<()> {
    // This initializes the env variables. On a normal day, I would have used dotenvy crate like in one of my past projects but this one feels much lighter for my environment.
    // Going forward, I'll use I, Me, My, We, Us, Ourselves and Our interchangeably as this codebase is for everyone and myself.
    // Quick story on this one, because it explains a real bug. Plain env_logger::init()
    // only ever looks at the RUST_LOG environment variable. Meanwhile daemon_log_level in
    // config.json gets parsed and saved just fine by cli/config.rs, but nobody ever wired
    // it into the actual logger, so it just sat there doing nothing. On top of that,
    // env_logger's default filter when RUST_LOG isn't set only lets error! calls through.
    // So every info!/warn!/debug! line in the daemon, things like "bruh daemon starting"
    // or "Flushing N events" or even a "Flush failed" warning, was getting silently
    // dropped. That's the whole reason daemon.log could sit there completely empty even
    // while the daemon was alive and doing real work. The fix below reads daemon_log_level
    // from config and hands it to env_logger as the default filter, so logging finally
    // reflects what the config file actually says. If someone has RUST_LOG set by hand,
    // we still respect that first, since that's a more explicit signal than the config.
    let log_level = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        cli::Config::load()
            .map(|c| c.daemon_log_level)
            .unwrap_or_else(|_| "info".into())
    });
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();
    // This is for us to receive arguments from the command line when cargo run, build or the command `bruh` is used. We collect the arguments in a Vector containing String values. Remember that I could have used the clap crate for all these but for memory.
    let args: Vec<String> = std::env::args().collect();
    // We then extract the index[1] command in the arguments to see if it matches any of the constant array of known commands we listed somewhere at the top of this file. // The actual first one(index[0]) is of course the command `bruh` but that doesn't belong to commands grouped as arguments here. In clap's terminology, we would say subcommand in certain instances.
    let first = args.get(1).map(|s| s.as_str());

    // Here, if index[[1] is not a known command, we treat everything after the binary name as a natural-language query. Let me tell you briefly what led to this decision. I wanted something that sounded natural. At first, the design I had was something like `bruh query "what was the last thing I fixed?"`but then I realised we could just write `bruh "what was the last thing I fixed?"` without the query. This led to my checking for the first word.
    if let Some(f) = first {
        // We are basically saying, if index[1] is not contained in the known commands and also does not have the - (short option) symbol in it, we should take everything after it as a query.
        // Take note that we extracted the f value from first in the if let Option above.
        //  The reason is that the .get(1) method in the "let first variable assignment" returned an Option which we have to match to extract; `if let` is a happy form of matching and extracting. The reason for using the get method instead of accessing the index directly is that it returns an Option type so that the program doesn't just panic each time we somehow try to access something that is out of the scope of the args vector and of course we can decide what happens if it returns none using a match. In this case, it will panic but that's almost IMPOSSIBLE to happen :-)
        if !KNOWN_CMDS.contains(&f) && !f.starts_with('-') {
            let query = args[1..].join(" ");
            // Here, we check for the --raw flag that will toggle between machine and human-readable displays.
            let raw = args.contains(&"--raw".to_string());
            // We then remove the raw flag here and take the clean query.
            let query_clean = query.replace("--raw", "").trim().to_string();

            //  The cli::query::run() then gets the parameters passed to it: the cleaned query and the state of being raw or not(true or false).
// We will pass this to cognee in query run then pass the response to the printer in cli output for formatting.
// Literally, the raw state is not used by the run itself but the cli ouput printer. This only serves as a portal to pass it as a pair on each command entry.


// QUICK ONE: I add some comments as the project grows. Just figure it out.
            return cli::query::run(&query_clean, raw).await;
        }
    }

    // This is where we do a lot of file travelling. Apart from the return expression above, this is the main communication portal between the main file and every other file within the whole project.
    // It basically matches the index[1] we got all along to the proper module it belongs.
    // Recall that the "first" variable used the get method which returns an Option type, and that's why we're still using the extraction with the Some() variant.
    // In special cases where we have a flag or <option> to deal with, we will resolve that within the match arm itself.
    match first {
        Some("init") => {
            //  Note that --force reinstalls the git hook even if already present.
            if args.iter().any(|a| a == "--force") {
                cli::init::run_force()?; // Now here's the first time we're propagating with the question mark operator without counting the cli::query::run() above cause it will definitely return the Result Type by the end of the day.
            } else {
                cli::init::run()?; // This satisfies the init command
            }
        }

        Some("daemon") => {
            // The daemon is the first heart of this project. Without it, the project would be a recurring burden to developers

            // Just like we did with --force in init, we do same with daemon options for --status
            if args.iter().any(|a| a == "--status") {
            // This function will literally just tell us if the daemon is actively working or not then close the program(return).
                return cli::status::run().map_err(Into::into);
            }
            // Without the flag, it sets up the daemon.
            info!("Starting bruh daemon…");
            daemon::run().await?;
        }
        // Say a developer is oblivious of the natural language design, this is a provision for it to take the word "query" as an arguments
        // Wait, I just had an idea now!!! We could have a command where the user queries cognee for information then ports the response to an LLM with a custom prompt for something specific. That will be cool in future but eyes on the goal now.
        Some("query") => {
        // As usual, we check for the flags; here's raw and interactive.
            let raw = args.contains(&"--raw".to_string());
            let interactive = args.iter().any(|a| a == "--interactive" || a == "-i");

            if interactive {
                cli::query::run_interactive().await?;
            } else {
                // We then collect everything after "query" (and after any flags) as the query
                let q: Vec<&str> = args[2..]
                    .iter()
                    .filter(|a| !a.starts_with("--"))
                    .map(|s| s.as_str())
                    .collect();
                    // If there's nothing after the flags, it means we don't have a query so we print a useful guide.
                    // Note that the bail macro in anyhow has the return function embedded in it so the program will print the guideline, and exit.
                if q.is_empty() {
                    anyhow::bail!("Usage: bruh query <text>  |  bruh query --interactive");
                }
                // After all these, if there's a query, we pass it to run.
                // We're referencing instead of passing ownership rather. Remember why? Right! Cause Rust does not allow taking ownerhip of a vec index and we want to be sure the whole lump is fairly settled here.
                // As usual, raw is a state of true or false.
                cli::query::run(&q.join(" "), raw).await?;
            }
        }
        

        Some("forget") => {
            // We state here that the none we need for the before and session is of an advanced String Type but default the value to none while keeping them mutable.
            // Basically, these are all defaults for situations where the options are not stated by the commander.
            // That would mean, without the options, there's no before and session. There's also no forced valuation(set to false).
            let mut before: Option<String> = None;
            let mut session: Option<String> = None;
            let mut force = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--before" => {
                        i += 1;
                        before = args.get(i).cloned();
                    }
                    "--session" => {
                        i += 1;
                        session = args.get(i).cloned();
                    }
                    "--force" => force = true,
                    _ => {}
                }
                i += 1;
            }
            cli::forget::run(before, session, force).await?;
        }

        //  Apart from being a daemon, this program will be self-learning. This means if it encounters a command it doesn't recognize (a package name), it will go online to search for what it is, figure it out and know it.
        // It probably will be able to do so on its own but this feature will be here in a situation where we want to force it or better said 'coerce' it to do so.
        // We initialize learn to nothing while expecting a String Type. Then we look for the --learn  flag. If we find it, we seek for the following argument and pass it to the managers::run associating function.
        Some("managers") => {
            let mut learn: Option<String> = None;
            let mut i = 2;
            // I can see an edge case here in this while loop but I'll handle that later. Let's see how the internal workings will be first.
            while i < args.len() {
                if args[i] == "--learn" {
                    i += 1;
                    learn = args.get(i).cloned();
                }
                i += 1;
            } 
            cli::managers::run(learn).await?;
        }
        // The four wise men below are straightforward, aren't they?
        
        Some("stats") | Some("--stats") => {
            cli::stats::run().await?;
        }
        Some("providers") => {
            cli::providers::run().await?;
        }

        Some("explain") => {
            cli::explain::run().await?;
        }
        
        Some("improve") => {
            cli::improve::run().await?;
        }

        
        Some("watch") => {
            // We take the third argument(index[2]) and pass it to the watch runner as a reference.
            // The fun fact here is that we cannot take ownership of a vec index according to Rust rules right? So we reference it! Is that contextually sound here?
            let cmd_args = args[2..].to_vec();
            cli::watch::run(&cmd_args).await?;
        }
        // The config command needs awareness of three values.
        // We'll have a config list, set and get.
        // List goes in by default if none is provided
        // Set goes with no arguments key value pairs and get will just need the key to get the value according to the design.
        // The sub means subcommands and list goes in by default with no required arguments.
        Some("config") => {
            let sub = args.get(2).map(|s| s.as_str()).unwrap_or("list");
            let key = args.get(3).map(|s| s.as_str());
            let value = args.get(4).map(|s| s.as_str());
            cli::config_cli::run(sub, key, value)?;
        }
        
        // This helps users check the version of the package and git hash for it. It will be helpful in future for debugging and security verifications.
        Some("version") | Some("--version") | Some("-v") => {
            println!(
                "{} {} ({})",
                cli::output::bold("bruh"),
                env!("CARGO_PKG_VERSION"),
                cli::output::dim(env!("GIT_HASH"))
            );
        }
        // Let me point out something in this design. Remember that when the user does not use any of the command word or a flag, we take the string after the bruh command as a query. Now, the word 'help' is a very common verb and a lot of users will want to use it in their queries. For instance: `bruh help me check the last error`.If something like that happens, it will confuse the match parser and for that reason, we will not use the help option without the flag as commented out below. Anyone that needs help must use the flag or make some command errors.
        /*  Some("help") | */
        Some("--help") | Some("-h") | None => {
            print_help();
        }
        // This is where we catch-all. It's designed to be annoying when you get the commands wrong everytime. What we do is, whether the user calls the program with no arguments or with misunderstood queries, we print the error, and the help message. Then exit the program with a value that's NOT 0.
        Some(unknown) => {
            eprintln!(
                "{} {}",
                cli::output::orange("Unknown command:"),
                unknown
            );
            print_help();
            std::process::exit(1);
        }
    }
        // Whew!!! That's it for the Parser. Now let's get the data flowing!
        
    Ok(()) // When the function ends successfully, it returns this to satisfy the return contract.
}

// We will print this basically when the program is run without valid arguments.
// It should be in a module of its own but it's not a crime here either.
// I know these are a lot of prints and it looks hazy but it's all good! I'm also aware of the memory cost of using the println macro though.
fn print_help() {
    use cli::output::{bold, cyan, dim, green};

    // One helper for the repeated "  command   description" row shape below, keeps every
    // line's spacing consistent without hand-aligning 15 different println! calls.
    let row = |cmd: &str, desc: &str| {
        println!("  {}  {}", green(&format!("{:<36}", cmd)), dim(desc));
    };

    println!("{} {}\n", bold(&cyan("bruh")), dim("— persistent developer memory"));
    println!("{}", bold("USAGE:"));
    row("bruh <query>", "Natural language memory query (shorthand)");
    println!("  {} [options]\n", bold(&cyan("bruh <command>")));
    println!("{}", bold("COMMANDS:"));
    row("init", "Set up bruh (API keys, git hook, autostart)");
    row("daemon", "Start background daemon");
    row("daemon --status", "Show daemon health");
    row("query <text> [--raw] [--interactive]", "Query memory");
    row("explain", "Session handoff brief for current directory");
    row("watch <cmd> [args...]", "Run command; surface error history on failure");
    row("stats", "Productivity summary");
    row("improve", "Trigger Cognee graph enrichment");
    row("forget --before <date>", "Forget events before date");
    row("forget --session <id> [--force]", "Forget a session");
    row("managers", "List known package managers");
    row("managers --learn <name>", "Learn a new package manager");
    row("providers", "Show LLM provider status");
    row("config list", "Show all configuration");
    row("config get <key>", "Get a config value");
    row("config set <key> <value>", "Set a config value");
    row("version", "Show version");
    println!();
    println!("{}", bold("ENV VARS:"));
    println!("  {}  {}", cyan(&format!("{:<20}", "BRUH_COGNEE_API_KEY")), dim("Override Cognee API key"));
    println!("  {}  {}", cyan(&format!("{:<20}", "BRUH_POLL_INTERVAL")), dim("Override poll interval (seconds)"));
    println!("  {}  {}", cyan(&format!("{:<20}", "NO_COLOR")), dim("Disable ANSI colors"));
    println!("  {}  {}", cyan(&format!("{:<20}", "RUST_LOG")), dim("Log level (info, debug, warn)"));
}

// There are no tests to run here for now. Let's see how it goes towards the end of the journey.
// I'll now go ahead to engage with each of the connected modules to ensure a round flow of data. Obviously, we'll start with the cli::query cause why not?
