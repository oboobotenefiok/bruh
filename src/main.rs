// Thanks for opting to read through my code base

// THIS CODEBASE HAS VERY VERBOSE COMMENTS AS IT IS TAILORED FOR LEARNING PURPOSES

// I'll be explaining every line of code contextually while building.
// I hope this serves as a great learning point for you.

// There used to be a blanket #![allow(dead_code, unused_imports, unused_variables)] right
// here, added early on to keep the hackathon build quiet while things were still moving
// fast. It's gone now. The problem with silencing those lints crate-wide is that they're
// exactly the ones that catch a config value you parsed but forgot to wire up, or an old
// helper nobody calls anymore, real bugs, not just noise. Anything that legitimately needs
// to stay unused for now gets a scoped #[allow(...)] right on that item with a comment
// explaining why, so the rest of the crate keeps the safety net intact.

// Based on my project plan, these are the modules I'll primarily need for everything to be complete. I declare them using mod. If anything else comes up as I iterate, I'll add them here.
mod cli;
mod cognee;
mod daemon;
mod discovery;
mod events;

// At first I wanted using thiserror crate to experiment but I've come to realise I haven't used it much before now and there's no need taking such risk. I'll stay with anyhow for now.
use anyhow::Result;
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

// Commands that only ever take flags, never freeform positional text. Paired with each is
// the exact set of flags that command actually recognises. This exists to catch a real
// ambiguity in the natural-language shorthand: if someone types `bruh daemon seems stuck`
// without quoting it, the shell hands us ["daemon", "seems", "stuck"] as three separate
// words, and "daemon" alone is indistinguishable from someone genuinely typing the daemon
// subcommand. Since none of these commands ever expect stray positional words, seeing any
// is a strong signal the whole thing was meant as a query, not a subcommand invocation.
//
// config/managers/forget/watch/query are deliberately left out of this list: they
// legitimately take positional arguments as part of normal usage (a config key and value,
// a package manager name, a command to run), so "extra positional text" is expected and
// correct for them, not a sign of misrouting.
const FLAGS_FOR_BARE_CMD: &[(&str, &[&str])] = &[
    ("init", &["--force"]),
    ("daemon", &["--status", "--flush-now"]),
    ("stats", &[]),
    ("providers", &[]),
    ("explain", &[]),
    ("improve", &[]),
    ("version", &[]),
];

// True if every argument after the subcommand word is one of the flags that subcommand
// actually accepts (or there are no extra arguments at all). See FLAGS_FOR_BARE_CMD above
// for why this matters.
fn looks_like_bare_subcommand(args: &[String], allowed_flags: &[&str]) -> bool {
    args[2..]
        .iter()
        .all(|a| allowed_flags.contains(&a.as_str()))
}

// Looks for `flag` in args and returns whatever comes right after it. Used for anything
// that takes a value, --before, --session, --learn, instead of each command hand-rolling
// its own little index-walking loop to do the exact same thing.
fn extract_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

// Just checks whether a bare flag like --force or --raw showed up anywhere in the args.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

// Pulls --raw and --interactive/-i out of a raw argument slice and returns the leftover
// words joined back into one query string, along with the two flag states. Both the
// shorthand `bruh "<query>"` path and the explicit `bruh query <text>` path call this now,
// so there's exactly one implementation of "how do we recognise flags in a query" instead
// of two that quietly did it differently: the shorthand path used to strip "--raw" with a
// plain string replace (which would mangle a query that happened to contain that substring
// as ordinary text) and never stripped --interactive/-i at all, so `bruh "my query"
// --interactive` silently didn't do what the equivalent `bruh query "my query"
// --interactive` did.
fn parse_query_args(args: &[String]) -> (String, bool, bool) {
    let raw = has_flag(args, "--raw");
    let interactive = has_flag(args, "--interactive") || has_flag(args, "-i");
    let text = args
        .iter()
        .filter(|a| !matches!(a.as_str(), "--raw" | "--interactive" | "-i"))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    (text, raw, interactive)
}

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

    // Here, if index[1] is not a known command, we treat everything after the binary name as a natural-language query. Let me tell you briefly what led to this decision. I wanted something that sounded natural. At first, the design I had was something like `bruh query "what was the last thing I fixed?"` but then I realised we could just write `bruh "what was the last thing I fixed?"` without the query. This led to my checking for the first word.
    if let Some(f) = first {
        // Two ways this counts as a query instead of a real subcommand:
        //   1. The first word just isn't a known command at all (the common case, someone
        //      quoted their whole question like the docs show).
        //   2. The first word DOES match a known command, but it's one that only ever takes
        //      flags (see FLAGS_FOR_BARE_CMD), and there's stray non-flag text after it.
        //      That combination can only happen if someone typed an unquoted question that
        //      happens to start with a reserved word, like `bruh daemon seems stuck`, since
        //      a real invocation of that subcommand would never have extra freeform words
        //      dangling off the end.
        let is_reserved = KNOWN_CMDS.contains(&f);
        let misrouted_reserved_word = is_reserved
            && FLAGS_FOR_BARE_CMD
                .iter()
                .find(|(cmd, _)| *cmd == f)
                .is_some_and(|(_, flags)| !looks_like_bare_subcommand(&args, flags));

        if (!is_reserved && !f.starts_with('-')) || misrouted_reserved_word {
            let (query_clean, raw, interactive) = parse_query_args(&args[1..]);
            if interactive {
                return cli::query::run_interactive().await;
            }
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
            if has_flag(&args, "--force") {
                cli::init::run_force()?; // Now here's the first time we're propagating with the question mark operator without counting the cli::query::run() above cause it will definitely return the Result Type by the end of the day.
            } else {
                cli::init::run()?; // This satisfies the init command
            }
        }

        Some("daemon") => {
            // The daemon is the first heart of this project. Without it, the project would be a recurring burden to developers

            // Just like we did with --force in init, we do same with daemon options for --status
            if has_flag(&args, "--status") {
                // This function will literally just tell us if the daemon is actively working or not then close the program(return).
                return cli::status::run().map_err(Into::into);
            }
            if has_flag(&args, "--flush-now") {
                return cli::status::force_flush().map_err(Into::into);
            }
            // Without the flag, it sets up the daemon.
            info!("Starting bruh daemon…");
            daemon::run().await?;
        }
        // Say a developer is oblivious of the natural language design, this is a provision for it to take the word "query" as an arguments
        // Wait, I just had an idea now!!! We could have a command where the user queries cognee for information then ports the response to an LLM with a custom prompt for something specific. That will be cool in future but eyes on the goal now.
        Some("query") => {
            // Same parse_query_args helper the shorthand path above uses, so both ways of
            // asking a question behave identically instead of two subtly different parsers.
            let (query_clean, raw, interactive) = parse_query_args(&args[2..]);

            if interactive {
                cli::query::run_interactive().await?;
            } else {
                // If there's nothing left after stripping flags, there's no actual query,
                // so we print a usage guide instead of sending an empty string to Cognee.
                // anyhow::bail! both builds the error and returns early in one step.
                if query_clean.is_empty() {
                    anyhow::bail!("Usage: bruh query <text>  |  bruh query --interactive");
                }
                cli::query::run(&query_clean, raw).await?;
            }
        }

        Some("forget") => {
            // Same extract_flag_value/has_flag helpers used everywhere else now, instead of
            // a hand-rolled loop walking the args by index.
            let before = extract_flag_value(&args, "--before");
            let session = extract_flag_value(&args, "--session");
            let force = has_flag(&args, "--force");
            cli::forget::run(before, session, force).await?;
        }

        //  Apart from being a daemon, this program will be self-learning. This means if it encounters a command it doesn't recognize (a package name), it will go online to search for what it is, figure it out and know it.
        // It probably will be able to do so on its own but this feature will be here in a situation where we want to force it or better said 'coerce' it to do so.
        // We initialize learn to nothing while expecting a String Type. Then we look for the --learn  flag. If we find it, we seek for the following argument and pass it to the managers::run associating function.
        Some("managers") => {
            // extract_flag_value handles the "flag with no value after it" case cleanly too
            // (args.get(i + 1) just returns None), no loop to get subtly wrong.
            let learn = extract_flag_value(&args, "--learn");
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
            // We take everything after "watch" and pass it to the watch runner.
            // On the question left here before: args[2..] is a slice we're borrowing, not
            // something we own outright, so we can't move a String out of it directly. What
            // .to_vec() actually does is auto-ref that slice and clone every element into a
            // brand new, owned Vec<String>, it's a real (small) allocation and clone, not a
            // free reference. For a handful of CLI args that cost is nothing, and it's the
            // correct, idiomatic way to turn a borrowed slice into owned data you can hand
            // off to something else, like Command::args() below in watch.rs.
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
            eprintln!("{} {}", cli::output::orange("Unknown command:"), unknown);
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

    println!(
        "{} {}\n",
        bold(&cyan("bruh")),
        dim("— persistent developer memory")
    );
    println!("{}", bold("USAGE:"));
    row("bruh <query>", "Natural language memory query (shorthand)");
    println!("  {} [options]\n", bold(&cyan("bruh <command>")));
    println!("{}", bold("COMMANDS:"));
    row("init", "Set up bruh (API keys, git hook, autostart)");
    row("daemon", "Start background daemon");
    row("daemon --status", "Show daemon health");
    row("daemon --flush-now", "Force a flush and reset backoff state");
    row("query <text> [--raw] [--interactive]", "Query memory");
    row("explain", "Session handoff brief for current directory");
    row(
        "watch <cmd> [args...]",
        "Run command; surface error history on failure",
    );
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
    println!(
        "  {}  {}",
        cyan(&format!("{:<20}", "BRUH_COGNEE_API_KEY")),
        dim("Override Cognee API key")
    );
    println!(
        "  {}  {}",
        cyan(&format!("{:<20}", "BRUH_POLL_INTERVAL")),
        dim("Override poll interval (seconds)")
    );
    println!(
        "  {}  {}",
        cyan(&format!("{:<20}", "NO_COLOR")),
        dim("Disable ANSI colors")
    );
    println!(
        "  {}  {}",
        cyan(&format!("{:<20}", "RUST_LOG")),
        dim("Log level (info, debug, warn)")
    );
}

// There are no other tests to run here beyond the ones right below. Let's see how it goes towards the end of the journey.
// I'll now go ahead to engage with each of the connected modules to ensure a round flow of data. Obviously, we'll start with the cli::query cause why not?

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extract_flag_value_finds_the_value_after_the_flag() {
        let a = args(&["bruh", "forget", "--before", "2026-01-01"]);
        assert_eq!(
            extract_flag_value(&a, "--before"),
            Some("2026-01-01".to_string())
        );
    }

    #[test]
    fn extract_flag_value_missing_flag_is_none() {
        let a = args(&["bruh", "forget", "--force"]);
        assert_eq!(extract_flag_value(&a, "--before"), None);
    }

    #[test]
    fn extract_flag_value_flag_with_no_trailing_value_is_none() {
        // --learn as the very last argument, nothing after it to grab.
        let a = args(&["bruh", "managers", "--learn"]);
        assert_eq!(extract_flag_value(&a, "--learn"), None);
    }

    #[test]
    fn has_flag_detects_presence_and_absence() {
        let a = args(&["bruh", "forget", "--force"]);
        assert!(has_flag(&a, "--force"));
        assert!(!has_flag(&a, "--before"));
    }

    #[test]
    fn parse_query_args_strips_raw_as_a_token_not_a_substring() {
        // This is the exact bug that used to exist: a plain string .replace("--raw", "")
        // would mangle a query containing that substring anywhere, not just as a flag.
        let a = args(&["explain", "the", "--raw", "flag", "behavior"]);
        let (text, raw, interactive) = parse_query_args(&a);
        assert_eq!(text, "explain the flag behavior");
        assert!(raw);
        assert!(!interactive);
    }

    #[test]
    fn parse_query_args_strips_interactive_long_and_short_form() {
        let a = args(&["my", "query", "--interactive"]);
        let (text, _, interactive) = parse_query_args(&a);
        assert_eq!(text, "my query");
        assert!(interactive);

        let a = args(&["my", "query", "-i"]);
        let (text, _, interactive) = parse_query_args(&a);
        assert_eq!(text, "my query");
        assert!(interactive);
    }

    #[test]
    fn parse_query_args_plain_query_is_untouched() {
        let a = args(&["what", "was", "the", "last", "thing", "I", "fixed"]);
        let (text, raw, interactive) = parse_query_args(&a);
        assert_eq!(text, "what was the last thing I fixed");
        assert!(!raw);
        assert!(!interactive);
    }

    #[test]
    fn looks_like_bare_subcommand_true_for_no_extra_args() {
        let a = args(&["bruh", "daemon"]);
        assert!(looks_like_bare_subcommand(&a, &["--status"]));
    }

    #[test]
    fn looks_like_bare_subcommand_true_for_a_recognised_flag() {
        let a = args(&["bruh", "daemon", "--status"]);
        assert!(looks_like_bare_subcommand(&a, &["--status"]));
    }

    #[test]
    fn looks_like_bare_subcommand_false_for_stray_words() {
        // This is the unquoted-collision case: "daemon" is a real subcommand, but "seems"
        // and "stuck" aren't --status, so this was never really meant as the daemon command.
        let a = args(&["bruh", "daemon", "seems", "stuck"]);
        assert!(!looks_like_bare_subcommand(&a, &["--status"]));
    }

    #[test]
    fn reserved_word_collision_is_detected_for_bare_commands() {
        // Mirrors the exact check main.rs's shorthand-query branch does before dispatching.
        for word in ["daemon", "stats", "providers", "explain", "improve", "init"] {
            let a = args(&["bruh", word, "totally", "unrelated", "words"]);
            let f = a[1].as_str();
            let is_reserved = KNOWN_CMDS.contains(&f);
            assert!(is_reserved, "{word} should be a known command");
            let misrouted = FLAGS_FOR_BARE_CMD
                .iter()
                .find(|(cmd, _)| *cmd == f)
                .is_some_and(|(_, flags)| !looks_like_bare_subcommand(&a, flags));
            assert!(
                misrouted,
                "{word} followed by stray words should be treated as a query"
            );
        }
    }

    #[test]
    fn config_is_not_subject_to_the_bare_subcommand_check() {
        // config legitimately takes positional args (sub/key/value), so it's deliberately
        // absent from FLAGS_FOR_BARE_CMD, extra words after it are normal, expected usage.
        assert!(!FLAGS_FOR_BARE_CMD.iter().any(|(cmd, _)| *cmd == "config"));
        assert!(!FLAGS_FOR_BARE_CMD.iter().any(|(cmd, _)| *cmd == "managers"));
        assert!(!FLAGS_FOR_BARE_CMD.iter().any(|(cmd, _)| *cmd == "forget"));
        assert!(!FLAGS_FOR_BARE_CMD.iter().any(|(cmd, _)| *cmd == "watch"));
    }
}

