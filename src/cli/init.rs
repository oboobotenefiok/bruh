//! INIT-NEW-001: daemon autostart.  INIT-NEW-002: API key validation.
//! POLISH-004: Windows shell profile + conditional chmod.
//! GIT-005: --force flag reinstalls the hook.
// This is the onboarding wizard, the first thing a new user runs. It walks through getting
// a Cognee API key configured, checking which LLM providers are available for discovery,
// installing the git hook so commits get picked up in real time, and optionally wiring the
// daemon to autostart whenever a terminal opens. Everything here is meant to be forgiving:
// skip a step and bruh should still work, just with reduced functionality (no discovery
// without an LLM key, no real-time git without the hook, etc), rather than refusing to run
// at all.

use crate::cli::{
    output::{bold as b, dim as d, green as g, orange},
    Config,
};
use anyhow::{Context, Result};
use std::{
    env,
    io::{self, Write},
};

pub fn run() -> Result<()> {
    run_with_force(false)
}

/// GIT-005: --force reinstalls the git hook even if already present.
pub fn run_force() -> Result<()> {
    run_with_force(true)
}

// The actual wizard. Broken into clearly marked sections (API key, LLM providers, git hook,
// autostart, save) that each print their own status as they go, so even if something later
// fails the user can see exactly how far it got and what succeeded already.
fn run_with_force(force: bool) -> Result<()> {
    println!("\n{}", b("  bruh init"));
    println!("{}\n", d("  Configuring persistent developer memory\n"));

    let mut config = Config::load()?;

    // ── Cognee API key ─────────────────────────────────────────────
    // We check the environment first since someone might already have COGNEE_API_KEY set
    // globally, then fall back to whatever's already saved in config (unless --force is
    // set, in which case we always re-prompt), and only ask the user to type one in as a
    // last resort.
    let existing_key = env::var("COGNEE_API_KEY")
        .or_else(|_| env::var("BRUH_COGNEE_API_KEY"))
        .unwrap_or_default();

    let api_key = if !existing_key.is_empty() {
        println!("  {} Found COGNEE_API_KEY in environment", g("✓"));
        existing_key
    } else if !config.cognee_api_key.is_empty() && !force {
        println!("  {} Cognee API key already configured", g("✓"));
        config.cognee_api_key.clone()
    } else {
        println!("  {} Cognee API key not set.", orange("○"));
        println!("    Get one at: {}", b("https://app.cognee.ai"));
        print!("  Enter your Cognee API key (blank to skip): ");
        io::stdout().flush()?;
        let mut key = String::new();
        io::stdin().read_line(&mut key)?;
        key.trim().to_string()
    };

    if !api_key.is_empty() {
        config.cognee_api_key = api_key.clone();
        // INIT-NEW-002: validate key works
        // We do a live check against Cognee before saving, so the user finds out
        // immediately if they fat-fingered the key rather than discovering it hours later
        // when the daemon's first flush silently fails.
        print!("  Validating API key… ");
        io::stdout().flush()?;
        match validate_cognee_key(&api_key, &config.cognee_api_url) {
            Ok(()) => println!("{}", g("✓")),
            Err(e) => {
                println!("{}", orange("✗"));
                println!("  {} Validation failed: {}", orange("!"), e);
                println!(
                    "    Fix later: {}",
                    b("bruh config set cognee_api_key <key>")
                );
            }
        }
    } else {
        // No key at all means Cognee calls will just fail, so we disable discovery
        // upfront rather than letting the daemon repeatedly hit errors it has no chance
        // of recovering from.
        println!("  {} Skipping key — discovery disabled", d("–"));
        config.discovery_enabled = false;
    }

    // ── LLM providers ──────────────────────────────────────────────
    // Just a status check here, not a prompt, we can't really "set" these interactively
    // since they're API keys from three different companies, so we just tell the user
    // which ones we found configured in their environment already and point them at where
    // to get one for free if none are set.
    println!();
    let providers = [
        ("GOOGLE_AI_API_KEY", "gemini"),
        ("GROQ_API_KEY", "groq"),
        ("ANTHROPIC_API_KEY", "claude"),
    ];
    let mut found = 0usize;
    for (var, name) in &providers {
        if env::var(var).is_ok() {
            println!("  {} {} ({})", g("✓"), b(name), d(var));
            found += 1;
        } else {
            println!("  {} {} — set {} to enable", d("–"), name, d(var));
        }
    }
    if found == 0 {
        println!(
            "\n  {} Discovery disabled — configure a provider first.",
            orange("○")
        );
        println!(
            "    Free: {} or {}",
            b("aistudio.google.com"),
            b("console.groq.com")
        );
        config.discovery_enabled = false;
    }

    // ── Git hook ───────────────────────────────────────────────────
    println!();
    match install_git_hook(force) {
        Ok(true) => println!("  {} git post-commit hook installed", g("✓")),
        Ok(false) => println!("  {} Not in a git repo (hook skipped)", d("–")),
        Err(e) => println!("  {} Hook install failed: {}", orange("○"), e),
    }

    // ── INIT-NEW-001 / SHELL-006: daemon-supporting shell content ──
    // This used to be gated behind a "y/N" prompt, which meant the daemon could only ever
    // see whatever's already in your shell history file, since neither bash nor zsh write
    // to that file after every command by default (both only flush it when the shell
    // exits, unless something turns on incremental writes). So on a long-running terminal
    // session, the daemon would sit there polling a history file that never changes until
    // you close the window. That's most of why cargo/package events showed up reliably
    // (daemon/packages.rs polls Cargo.lock and the registry directly, it doesn't depend on
    // shell history at all) while plain shell commands and everything downstream of them
    // just didn't. install_shell_integration() below writes both the daemon autostart line
    // and the shell-specific "flush history immediately" directive in one go, no prompt,
    // so this is no longer something a user has to know to opt into.
    println!();
    match install_shell_integration(force) {
        Ok(Some(p)) => {
            println!("  {} Daemon + shell integration added to {}", g("✓"), b(&p));
            // Here's the thing worth spelling out for whoever's reading this later. We
            // just wrote a new PROMPT_COMMAND (or INC_APPEND_HISTORY for zsh) into that
            // profile file, but writing it to disk doesn't make it real yet. Your current
            // shell already loaded its profile a while ago and isn't going to notice this
            // change on its own. So if we don't say anything, the natural next step is
            // "run bruh daemon &, type a few commands, wonder why nothing's showing up,"
            // which is basically the whole bug this comment exists to prevent.
            println!(
                "  {} New shell setting won't apply to this session. Run {} or open a new terminal.",
                orange("○"),
                b(&format!("source {}", p))
            );
        }
        Ok(None) => {
            println!(
                "  {} Daemon + shell integration already present, skipped",
                d("–")
            );
            // Same story as above, just for the "it's already there" path. Someone could
            // easily be sitting in the exact same shell session that was open the first
            // time bruh init ever ran, in which case the incremental-flush setting has
            // never actually been loaded, even though the line has been sitting in their
            // profile the whole time. Worth a nudge rather than letting them assume it's
            // working.
            println!(
                "  {} If commands still aren't showing up, make sure this shell has re-sourced its profile since bruh init first ran.",
                orange("○")
            );
        }
        Err(e) => println!("  {} Could not update shell profile: {}", orange("○"), e),
    }

    // ── Save ───────────────────────────────────────────────────────
    config.save()?;
    println!("\n  {} Config saved", g("✓"));
    println!("  {} Run {}\n", b("→"), b("bruh daemon &"));
    Ok(())
}

// Rather than requiring a "real" auth endpoint, we just hit Cognee's /health with the
// bearer token attached and treat anything under a server error (500+) as "the key is at
// least accepted enough to reach the server." Connection errors or timeouts get treated as
// "can't tell, assume it's fine" rather than "the key is bad", since a flaky network
// shouldn't make init look like it failed over a perfectly good key.
fn validate_cognee_key(key: &str, api_url: &str) -> Result<()> {
    // Let's talk through why this function needs the wrapper below, because it's a
    // sneaky one. bruh init runs inside an async main (that's what #[tokio::main] on
    // main.rs gives us), but this function is plain synchronous code that reaches for
    // reqwest::blocking to make one quick HTTP call. reqwest::blocking works by quietly
    // spinning up its own little tokio runtime behind the scenes and waiting on it. The
    // problem is, tokio doesn't allow you to build or tear down one runtime while
    // you're already running inside another one on the same thread. It's not just
    // unsupported, it will panic, and it panics every single time, not once in a while.
    // tokio::task::block_in_place is the sanctioned escape hatch for exactly this
    // situation. It tells the multi-threaded runtime "step aside, this next bit of code
    // is going to block, hand this task off to a different worker so nothing gets
    // wedged." That's all we need here since the outer runtime is already running with
    // the rt-multi-thread feature.
    tokio::task::block_in_place(|| {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("HTTP client build failed")?;
        let url = format!("{}/health", api_url.trim_end_matches('/'));
        match client
            .get(&url)
            .header("Authorization", format!("Bearer {}", key))
            .send()
        {
            Ok(r) if r.status().as_u16() < 500 => Ok(()),
            Ok(r) => anyhow::bail!("HTTP {}", r.status()),
            Err(e) if e.is_connect() || e.is_timeout() => Ok(()), // server unreachable ≠ bad key
            Err(e) => anyhow::bail!("{}", e),
        }
    })
}

// Copies the bundled hooks/post-commit script (embedded into the binary at compile time via
// include_str!, so there's no risk of the hook file going missing at runtime) into the
// repo's .git/hooks/post-commit. If we're not inside a git repo at all, git rev-parse fails
// and we just return Ok(false) rather than treating that as an error, since not being in a
// repo is a perfectly normal state, not a problem.
fn install_git_hook(force: bool) -> Result<bool> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output();
    let output = match out {
        Ok(o) if o.status.success() => o,
        _ => return Ok(false),
    };

    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let hooks_dir = format!("{}/hooks", git_dir);
    std::fs::create_dir_all(&hooks_dir)?;

    let hook_path = format!("{}/post-commit", hooks_dir);

    // GIT-005: skip if already installed (unless --force)
    // We check that the existing hook actually mentions "bruh" before treating it as
    // already installed, otherwise we'd risk silently clobbering someone's pre-existing
    // custom post-commit hook that has nothing to do with us.
    if !force && std::path::Path::new(&hook_path).exists() {
        let existing = std::fs::read_to_string(&hook_path).unwrap_or_default();
        if existing.contains("bruh") {
            return Ok(true); // already ours
        }
    }

    let hook_content = include_str!("../../hooks/post-commit");
    std::fs::write(&hook_path, hook_content)?;

    // chmod +x only makes sense on Unix, git hooks need to be executable there, but
    // Windows doesn't use the same permission bit model at all, so this whole block is
    // conditionally compiled out entirely on Windows rather than being a no-op at runtime.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }
    Ok(true)
}

/// INIT-NEW-001 / SHELL-006: install everything the daemon needs from the shell side.
/// Returns Ok(Some(path)) if something was written, Ok(None) if it was already there and
/// force wasn't set, so run_with_force() can tell the two apart in its status line.
// Everything bruh adds lives between a pair of marker comments, that's what makes --force
// safe: instead of trying to guess which lines are "ours" by matching fragments of text,
// we just look for the whole marked block and swap it out wholesale. It also means anyone
// reading their own .bashrc later can tell at a glance exactly what bruh touched and where
// it starts and ends, nothing sneaks in unmarked.
const BLOCK_START: &str = "# >>> bruh daemon (managed, do not edit between markers) >>>";
const BLOCK_END: &str = "# <<< bruh daemon (managed, do not edit between markers) <<<";

fn install_shell_integration(force: bool) -> Result<Option<String>> {
    #[cfg(windows)]
    {
        // PowerShell's history already gets written to ConsoleHost_history.txt after every
        // command by default (no bash/zsh-style "only on exit" gotcha to work around here),
        // so the Windows side of this only needs the daemon autostart line, nothing extra
        // to flush.
        let profile_dir = std::env::var("USERPROFILE")
            .map(|h| std::path::PathBuf::from(h).join("Documents/PowerShell"))
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        std::fs::create_dir_all(&profile_dir)?;
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        let block = format!(
            "{}\nStart-Process bruh -ArgumentList 'daemon' -WindowStyle Hidden\n{}\n",
            BLOCK_START, BLOCK_END
        );
        return write_managed_block(&profile, &block, force);
    }
    #[cfg(not(windows))]
    {
        // Defaulting to .bashrc unless SHELL clearly says zsh, this covers the two shells
        // the vast majority of people are actually running day to day.
        let shell = env::var("SHELL").unwrap_or_default();
        let home = crate::cli::config::home_dir();
        let (profile, history_fix) = if shell.contains("zsh") {
            (
                home.join(".zshrc"),
                // zsh's default is the same "only write history at shell exit" behavior as
                // bash. INC_APPEND_HISTORY turns that off, each command lands in
                // .zsh_history right after it runs, which is what the daemon actually
                // needs since it polls that file on a timer, not on shell exit.
                "# Write each command to .zsh_history immediately, bruh's daemon polls\n\
                 # this file on a timer and can only see what's actually been written.\n\
                 setopt INC_APPEND_HISTORY"
                    .to_string(),
            )
        } else {
            (
                home.join(".bashrc"),
                // Plain bash only appends to .bash_history when the shell exits, unless
                // something calls `history -a` more often. Chaining it onto PROMPT_COMMAND
                // (which bash runs before every prompt redraw, so effectively after every
                // command) means it fires constantly instead of once per session. The
                // `${PROMPT_COMMAND:+; $PROMPT_COMMAND}` part preserves whatever the user
                // already had in PROMPT_COMMAND rather than clobbering it.
                "# Flush each command to .bash_history immediately instead of waiting for\n\
                 # the shell to exit, bruh's daemon polls this file on a timer and can\n\
                 # only see what's actually been written to disk.\n\
                 export PROMPT_COMMAND=\"history -a${PROMPT_COMMAND:+; $PROMPT_COMMAND}\""
                    .to_string(),
            )
        };

        let block = format!(
            "{}\n{}\n# Only start the daemon if one isn't already running. Without this\n\
             # check, every new terminal you open would spawn another daemon process,\n\
             # and they'd all end up polling the same files and flushing to the same\n\
             # Cognee dataset at the same time, which is exactly the kind of collision\n\
             # bruh's client-side retry logic exists to paper over, better to just not\n\
             # create the collision in the first place.\n\
             if ! pgrep -x \"bruh\" > /dev/null 2>&1; then\n\
             \x20   mkdir -p ~/.local/share/bruh\n\
             \x20   nohup bruh daemon > ~/.local/share/bruh/daemon.log 2>&1 &\n\
             fi\n{}\n",
            BLOCK_START, history_fix, BLOCK_END
        );
        write_managed_block(&profile, &block, force)
    }
}

// Shared append-or-replace logic for both the Windows and Unix branches above. If the
// marked block isn't present yet, we append it. If it is present and force is set, we cut
// out the old block (markers included) and splice the new one in at the same spot rather
// than just appending a second copy underneath. If it's present and force isn't set, we
// leave the file untouched, that's the normal "already done, nothing to do" case on a
// plain re-run of `bruh init`.
fn write_managed_block(
    profile: &std::path::Path,
    block: &str,
    force: bool,
) -> Result<Option<String>> {
    let existing = std::fs::read_to_string(profile).unwrap_or_default();
    let already_present = existing.contains(BLOCK_START);

    if already_present && !force {
        return Ok(None);
    }

    let new_content = if already_present {
        // Cut everything from BLOCK_START to BLOCK_END (inclusive) and splice the fresh
        // block in its place, so re-running with --force updates the content in place
        // instead of leaving a stale copy above a new one.
        //
        // The expect() here is safe: we only reach this branch when already_present is
        // true, and already_present is set from existing.contains(BLOCK_START) a few lines
        // up, so find() on the same string for the same substring is guaranteed to return
        // Some.
        let start = existing.find(BLOCK_START).expect("BLOCK_START must be present, already_present just confirmed existing.contains(BLOCK_START)");
        let end = existing
            .find(BLOCK_END)
            .map(|i| i + BLOCK_END.len())
            .unwrap_or(existing.len());
        format!(
            "{}{}{}",
            &existing[..start],
            block,
            &existing[end..].trim_start_matches('\n')
        )
    } else {
        // Fresh install: just tack it onto the end with a blank line separating it from
        // whatever the user already had, one newline of breathing room, nothing more. If
        // the file's empty or didn't exist yet, skip the separator so we don't leave a
        // couple of pointless blank lines at the top of a brand new .bashrc.
        let trimmed = existing.trim_end_matches('\n');
        if trimmed.is_empty() {
            format!("{}\n", block)
        } else {
            format!("{}\n\n{}", trimmed, block)
        }
    };

    let mut f =
        std::fs::File::create(profile).with_context(|| format!("Cannot write {:?}", profile))?;
    f.write_all(new_content.as_bytes())?;
    // Explicit flush even though File::write_all already goes straight to the OS without
    // any userspace buffering of its own, this is just making the intent obvious to
    // anyone reading the code later: we want this fully on disk before init reports
    // success, not queued up and dropped if the process gets killed a moment later.
    f.flush()?;

    Ok(Some(profile.to_string_lossy().to_string()))
}

