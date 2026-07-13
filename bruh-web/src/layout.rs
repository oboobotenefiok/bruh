use maud::{html, Markup, DOCTYPE};

/// Wraps a page body in the shared head/nav/footer shell. Kept as one function (rather
/// than, say, separate header()/footer() calls the caller has to remember to invoke) so
/// there's exactly one place that can drift from the others, every page gets the same
/// chrome by construction.
pub fn page(title: &str, view_count: i64, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (title) }
                meta name="description" content="bruh is a background daemon and CLI that turns your shell history, git commits, and package installs into a queryable memory graph, so you can ask what you actually did.";
                link rel="stylesheet" href="/static/style.css";
                link rel="icon" href="data:,";
            }
            body {
                header class="site-header" {
                    div class="wrap header-inner" {
                        a class="brand" href="/" {
                            span class="brand-mark" { "☰" }
                            span { "bruh" }
                        }
                        nav class="site-nav" {
                            a href="#buffer-system" { "Buffer system" }
                            a href="#install" { "Install" }
                            a href="https://github.com/oboobotenefiok/bruh" target="_blank" rel="noopener" { "GitHub" }
                        }
                    }
                }
                main { (body) }
                footer class="site-footer" {
                    div class="wrap footer-inner" {
                        div class="footer-brand" {
                            span class="brand-mark small" { "☰" }
                            span { "bruh" }
                            p { "A background daemon that remembers what you actually did, so Cognee can answer for you later." }
                        }
                        div class="footer-links" {
                            div {
                                p class="footer-heading" { "Project" }
                                a href="https://github.com/oboobotenefiok/bruh" target="_blank" rel="noopener" { "Source" }
                            }
                            div {
                                p class="footer-heading" { "Status" }
                                p class="footer-status" {
                                    span class="dot" {}
                                    "Actively evolving"
                                }
                            }
                        }
                    }
                    div class="wrap footer-bottom" {
                        p { "© 2026 bruh. MIT licensed." }
                        p class="view-count" { (view_count) " views" }
                    }
                }
            }
        }
    }
}
