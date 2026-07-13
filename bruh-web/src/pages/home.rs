use maud::{html, Markup};

pub fn render() -> Markup {
    html! {
        (hero())
        (features())
        (buffer_system())
        (architecture())
        (install())
    }
}

fn hero() -> Markup {
    html! {
        section class="hero wrap" {
            p class="badge" {
                span class="badge-dot" {}
                "early and evolving"
            }
            h1 { "Your dev history, remembered." }
            p class="lede" {
                "bruh is a background daemon and CLI that ingests your shell history, git commits, "
                "and package installs into Cognee's hybrid graph-vector memory, so you can ask "
                "natural-language questions about what you actually did instead of digging through "
                "scrollback."
            }
            div class="hero-actions" {
                a class="btn btn-primary" href="https://github.com/oboobotenefiok/bruh" target="_blank" rel="noopener" {
                    "View on GitHub"
                }
                a class="btn btn-ghost" href="#buffer-system" { "See the buffer system" }
            }
            pre class="install-peek" {
                code {
                    "$ cargo install --path . \n"
                    "$ bruh init      " span class="comment" { "# installs the git hook, sets up the daemon" } "\n"
                    "$ bruh daemon    " span class="comment" { "# starts watching, batching every 30-60s" } "\n"
                    "$ bruh recall \"what did I ship last week?\""
                }
            }
        }
    }
}

fn features() -> Markup {
    let cards = [
        ("Shell history", "Every command, its exit code, and its working directory, reconstructed even across cd's, tagged with error type on failure."),
        ("Git commits", "A post-commit hook (with a poll-based fallback) captures hash, message, branch, and changed files the instant you commit."),
        ("Package events", "Installs and removals from your system's package manager, discovered and tracked without any manual configuration."),
        ("Natural-language recall", "remember(), recall(), improve(), and forget() give you a small, deliberate surface over a much bigger graph."),
    ];
    html! {
        section class="features wrap" {
            p class="eyebrow" { "What it watches" }
            h2 { "Four sources in, one memory graph out." }
            div class="card-grid" {
                @for (title, desc) in cards {
                    div class="card" {
                        h3 { (title) }
                        p { (desc) }
                    }
                }
            }
        }
    }
}

fn buffer_system() -> Markup {
    html! {
        section id="buffer-system" class="buffer-system wrap" {
            p class="eyebrow" { "Under the hood" }
            h2 { "Nothing gets lost when Cognee is unreachable." }
            p class="section-lede" {
                "Events are popped in bounded batches of 500, backlog first, so a failed send "
                "never turns into an all-or-nothing flood, and a retry never re-reads what "
                "already made it through."
            }

            div class="diagram" {
                div class="diagram-row diagram-sources" {
                    div class="diagram-box" {
                        span class="diagram-icon" { "▤" }
                        div {
                            strong { "buffer.ndjson" }
                            p { "new events" }
                        }
                    }
                    div class="diagram-box" {
                        span class="diagram-icon" { "↻" }
                        div {
                            strong { "buffer.backlog.ndjson" }
                            p { "failed events, retried first" }
                        }
                    }
                }
                div class="diagram-arrow" {}
                div class="diagram-row" {
                    div class="diagram-box diagram-box-wide" {
                        span class="diagram-icon" { "⚙" }
                        div {
                            strong { "pop_events(500)" }
                            ol {
                                li { "Read from the backlog first (oldest failures)" }
                                li { "Then from the primary buffer" }
                                li { "Return up to 500 events" }
                            }
                        }
                    }
                }
                div class="diagram-arrow" {}
                div class="diagram-row" {
                    div class="diagram-box diagram-box-wide diagram-box-accent" {
                        span class="diagram-icon diagram-icon-dark" { "♪" }
                        strong { "Send to Cognee" }
                    }
                }
                div class="diagram-arrow diagram-arrow-split" {}
                div class="diagram-row diagram-outcomes" {
                    div class="diagram-box" {
                        span class="diagram-icon diagram-icon-ok" { "✓" }
                        div {
                            strong { "ack_events()" }
                            p { "Advance cursor" }
                            p { "Truncate file once cursor reaches EOF" }
                        }
                    }
                    div class="diagram-box" {
                        span class="diagram-icon diagram-icon-fail" { "✕" }
                        div {
                            strong { "nack_events()" }
                            p { "Newly-failed events move to the backlog" }
                            p { "Already-backlogged ones stay put, no duplication" }
                        }
                    }
                }
            }
        }
    }
}

fn architecture() -> Markup {
    html! {
        section class="architecture wrap" {
            div class="architecture-grid" {
                div {
                    p class="eyebrow" { "Architecture" }
                    h2 { "A daemon that stays out of your way." }
                    p class="section-lede" {
                        "The daemon batches events every 30-60 seconds and ships them to the "
                        "Cognee Cloud REST API over reqwest. Nothing leaves your machine between "
                        "batches, and nothing is lost if a batch fails, it just waits its turn "
                        "in the backlog."
                    }
                }
                div class="status-box" {
                    p class="eyebrow" { "Current status" }
                    ul {
                        li { "Fresh codebase, actively developed" }
                        li { "Tests: 67 → 131 passing as of the last review pass, zero warnings" }
                        li { "Runs comfortably as a background process on Linux, macOS, and Termux" }
                    }
                }
            }
        }
    }
}

fn install() -> Markup {
    html! {
        section id="install" class="install wrap" {
            div class="install-box" {
                div {
                    h2 { "Ready to give it a shell?" }
                    p { "Clone it, build it, point it at your Cognee instance." }
                }
                a class="btn btn-primary" href="https://github.com/oboobotenefiok/bruh#readme" target="_blank" rel="noopener" {
                    "Read the setup docs"
                }
            }
        }
    }
}
