# bruh-web

The bruh landing site. A small Axum + Maud server, one
SQLite-backed view counter as the only bit of state. I love the MASH stack btw.

## Run it

```
cd web
cargo run
```

Serves on http://localhost:4321 by default (override with the `PORT` env var). The view
counter's SQLite file is created automatically at `web/data/bruh_web.db` on first run.

## Layout

- `src/main.rs` — Axum app, routes, DB wiring
- `src/db.rs` — the view-counter table (the one thing this site persists)
- `src/layout.rs` — shared header/nav/footer shell
- `src/pages/home.rs` — the homepage content, including the buffer-system diagram
- `static/style.css` — the whole stylesheet (cream/brown theme, no build step)

This is its own crate, deliberately not a workspace member of the root `bruh` binary, the
website has nothing to do with the daemon at runtime, so `cargo build` at the repo root is
unaffected by anything here.
