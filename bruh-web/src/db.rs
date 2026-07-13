use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

// WEB-001: the only thing this site actually needs a database for is a plain "how many
// times has the homepage loaded" counter, everything else on the page is static content
// baked into the binary via maud. Runtime-checked queries (sqlx::query, not the query!
// macro family) are used deliberately throughout this module rather than the compile-time
// checked macros, those need either a live database or a pre-generated offline cache
// available at *compile* time, which would make `cargo build` fail on a machine that's
// never run this before. A plain runtime query trades a small amount of compile-time safety
// for "always compiles, anywhere", the right call for a counter this simple.

/// Opens (creating if needed) the sqlite database this site's view counter lives in, and
/// makes sure the one table it needs exists.
pub async fn connect(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create database directory: {:?}", parent))?;
    }

    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .with_context(|| format!("Invalid sqlite path: {:?}", db_path))?
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .context("Failed to connect to the view-counter database")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS page_views (
            page TEXT PRIMARY KEY,
            views INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(&pool)
    .await
    .context("Failed to create page_views table")?;

    Ok(pool)
}

/// Increments the view count for `page` and returns the new total. `INSERT ... ON CONFLICT`
/// in one round trip keeps this atomic under concurrent requests, no read-then-write race
/// between two visitors landing at the same moment.
pub async fn record_view(pool: &SqlitePool, page: &str) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO page_views (page, views) VALUES (?, 1)
         ON CONFLICT(page) DO UPDATE SET views = views + 1
         RETURNING views",
    )
    .bind(page)
    .fetch_one(pool)
    .await
    .context("Failed to record page view")?;

    Ok(row.0)
}
