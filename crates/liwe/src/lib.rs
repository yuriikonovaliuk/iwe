#[cfg(feature = "djot")]
/// The iwe-plus version line every binary reports: this fork's version,
/// the upstream iwe release it is based on, and the commit it was built from.
pub const VERSION: &str = env!("IWE_VERSION_LINE");

pub mod djot;
pub mod format;
pub mod graph;
pub mod locale;
pub mod markdown;
pub mod model;
pub mod operations;
pub mod parser;
pub mod query;
pub mod schema;
pub mod state;
pub mod transaction;
pub mod write_lock;
