#!/usr/bin/env bash
# bruh demo script, DEMO-001
# This is the hackathon demo walkthrough, meant to actually show off why bruh is useful
# rather than just listing its features. Three scenarios, each one demonstrating a
# different kind of memory: a causal chain within one work session (build fails, you fix
# it, you commit, and bruh can explain the whole story later), memory that survives across
# sessions (a new shell still remembers what an old one did), and the self-learning
# discovery pipeline picking up a package manager it's never seen before, live, on stage.
# Requires: bruh daemon running, COGNEE_API_KEY set.
set -e

# Same little color-helper pattern as install.sh, plus two demo-specific helpers below.
bold()   { printf "\033[1m%s\033[0m\n" "$*"; }
dim()    { printf "\033[2m%s\033[0m\n" "$*"; }
green()  { printf "\033[32m%s\033[0m\n" "$*"; }
cyan()   { printf "\033[36m%s\033[0m\n" "$*"; }
yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
# Prints a section banner between scenarios so it is obvious to an audience watching where
# one demo beat ends and the next begins.
header() { echo ""; bold "── $* ──────────────────────────────────"; echo ""; }
# Echoes the command before running it (like a shell prompt would), then actually runs it.
# This is purely for the audience's benefit, so they can see what is being typed instead of
# just watching output appear with no idea what caused it.
run()    { dim "  \$ $*"; eval "$@"; }
# A named pause instead of a bare `sleep` scattered everywhere, mostly just for readability
# when skimming the script later to see where the deliberate dramatic beats are.
pause()  { sleep "${1:-2}"; }

clear
bold "  bruh — persistent developer memory"
dim  "  hackathon demo · WeMakeDevs × Cognee"
echo ""
dim  "  Prerequisites: bruh daemon running, COGNEE_API_KEY set"
echo ""

if ! command -v bruh >/dev/null 2>&1; then
    echo "bruh not found. Run install.sh first."
    exit 1
fi

# ── Start daemon if not running ───────────────────────────────────────────────
# We check the daemon's own status output rather than assuming it is or isn't running,
# so re-running this demo script twice in a row does not spawn a second daemon instance
# fighting the first one over the same offline buffer and cursor files.
if ! bruh daemon --status 2>/dev/null | grep -q "running"; then
    dim "  Starting daemon in background…"
    bruh daemon &>/tmp/bruh-demo.log &
    DAEMON_PID=$!
    sleep 3
    green "  ✓ Daemon started (PID $DAEMON_PID)"
fi

echo ""
read -rp "  Press Enter to begin demo…"

# ═══════════════════════════════════════════════════════════════════
header "SCENARIO 1: The Causal Chain"
dim  "  We build a project, hit a linker error, fix it, commit."
dim  "  bruh will reconstruct the full causal chain on query."
echo ""

# A throwaway git repo just for this demo, so we're not polluting the actual bruh repo (or
# whatever directory the person happens to be running this from) with fake commits.
DEMO_DIR=$(mktemp -d)
cd "$DEMO_DIR"
git init -q

pause 1
bold "  Simulating: cargo build (will fail — gcc not present)"
run "echo 'fn main(){}' > main.rs"
run "cargo build 2>&1 | head -5 || true"
pause 1

bold "  Simulating: install fix"
# We echo the apt install rather than actually running it, we don't want the demo to
# require real root access or actually touch the machine's package state, but the command
# text still lands in shell history either way, which is all bruh's shell poller needs to
# see to pick it up as a real event.
run "echo 'sudo apt install -y gcc build-essential'"
pause 1

bold "  Simulating: build success + commit"
run "echo '# fixed' >> main.rs"
run "git add -A && git commit -m 'fix: install gcc for linker'"
pause 2

# The daemon batches events and flushes on its own timer (default 60s, see
# batch_flush_interval_seconds in cli/config.rs) rather than sending immediately, so we
# have to actually wait here for the events we just generated to land in Cognee before
# querying for them, otherwise the query below would come back with nothing.
yellow "  Waiting 35 seconds for daemon flush…"
sleep 35

echo ""
bold "  Now querying: what did I do to fix that build error?"
echo ""
run "bruh 'what did I do to fix that linker error'"
pause 2

# ═══════════════════════════════════════════════════════════════════
header "SCENARIO 2: Cross-Session Query"
dim  "  We close the shell and open a new one."
dim  "  Memory persists — bruh knows what happened before."
echo ""

# Simulate a new shell by unsetting session-local vars
# We can't literally spawn a brand new terminal window from inside a script in a way an
# audience could watch, so this is a stand-in for that, the actual point being demonstrated
# is that bruh's recall() reaches back through Cognee's graph regardless of which shell
# session originally produced the memory, session_id is just metadata, not a query boundary.
unset DEMO_SESSION 2>/dev/null || true
pause 1

bold "  From a NEW terminal session — querying previous work:"
echo ""
run "bruh 'what packages did I install recently'"
pause 2

# ═══════════════════════════════════════════════════════════════════
header "SCENARIO 3: Discovery Portal"
dim  "  We use an unknown package manager (pnpm)."
dim  "  bruh detects it, searches the web, queries an LLM,"
dim  "  and adds it to its known managers — live."
echo ""

pause 1
bold "  Simulating: pnpm add react (unknown manager)"
run "echo 'pnpm add react'"
pause 1

bold "  Manually triggering discovery (daemon auto-discovers on next poll):"
# In real usage the daemon would notice "pnpm add" in shell history on its own next poll
# tick and kick off discovery silently in the background, see daemon/discovery.rs. For the
# demo we trigger it explicitly with --learn instead, since that path prints the whole
# cascade step by step (see cli/managers.rs's run_learn), which is much more interesting to
# watch live than a background task nobody can see happening.
echo ""
run "bruh managers --learn pnpm"
pause 2

echo ""
bold "  bruh managers — showing what bruh now knows:"
echo ""
run "bruh managers"
pause 1

# ═══════════════════════════════════════════════════════════════════
header "BONUS: Developer Stats"
echo ""
run "bruh stats"
pause 1

# ═══════════════════════════════════════════════════════════════════
header "BONUS: Context Brief (bruh explain)"
echo ""
run "bruh explain"

# ── Cleanup ───────────────────────────────────────────────────────────────────
cd /
rm -rf "$DEMO_DIR"

echo ""
green "  ✓ Demo complete"
dim   "  Get bruh: ${REPO:-https://github.com/oboobotenefiok/bruh}"
echo ""
