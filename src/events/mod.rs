// Small mod.rs here, nothing fancy. schema.rs holds the actual Event struct and its variants,
// this file just declares it as a submodule and re-exports everything from it with the
// wildcard so the rest of the codebase can do `use crate::events::Event` instead of having
// to reach all the way into `crate::events::schema::Event`. Saves a bit of typing everywhere else.
pub mod schema;
pub use schema::*;
