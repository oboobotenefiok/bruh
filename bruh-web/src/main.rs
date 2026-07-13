mod db;
mod layout;
mod pages;

use axum::{extract::State, response::Html, routing::get, Router};
use sqlx::SqlitePool;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tower_http::{compression::CompressionLayer, services::ServeDir, trace::TraceLayer};

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bruh_web=info,tower_http=info".into()),
        )
        .init();

    // WEB-002: db path is relative to wherever this binary is *run from*, which is fine,
    // this is a small self-hosted site, not something that needs to work from an arbitrary
    // cwd. `cd web && cargo run` (the documented way to run it) puts the db at
    // web/data/bruh_web.db, next to the crate, not scattered wherever the shell happened to
    // be when it launched.
    let db_path = PathBuf::from("data/bruh_web.db");
    let db = db::connect(&db_path).await?;
    let state = Arc::new(AppState { db });

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4321);

    let app = Router::new()
        .route("/", get(home))
        .route("/healthz", get(healthz))
        .nest_service("/static", ServeDir::new("static"))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("bruh-web listening on http://{addr}");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn home(State(state): State<Arc<AppState>>) -> Html<String> {
    // A failed view-count write shouldn't take the whole page down, worst case the counter
    // just doesn't move for this request. See db::record_view's own doc comment for why
    // this is a runtime query rather than one of sqlx's compile-time checked macros.
    let views = db::record_view(&state.db, "home").await.unwrap_or(0);
    let markup = layout::page("bruh — remember what you actually did", views, pages::home::render());
    Html(markup.into_string())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("Shutting down.");
}
