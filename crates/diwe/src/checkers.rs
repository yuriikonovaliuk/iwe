//! Compiled-in "always" checkers: unlike the external, config-driven
//! `[checkers.<name>]` mechanism in [`crate::schema::run_checkers`] (which
//! only runs when a checker's `always` flag is set, and only over a
//! transaction's touched keys), a checker registered here runs
//! unconditionally on every `[transactions] validate = "full"` commit —
//! no config toggle, no touched-keys gate. See
//! `crates/diwe/src/validating_transaction.rs`'s always-checkers list.

pub mod expire_suppressions;
