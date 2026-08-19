#![cfg(feature = "postgres")]
//! Flow-facing PostgreSQL app-data capability (`postgres.*`).
//!
//! Runs *named*, project-declared SQL operations on a dedicated pool that is
//! completely separate from the internal `StateStore` (own URL, role, schema).
//! See docs/postgres-app-data.md for the trust model: named operations bound
//! prompt-injected agents and reviewers, the database role bounds flow authors.

pub mod operations;
pub mod policy;

mod execute;
mod sql_split;

#[cfg(test)]
mod operations_tests;
#[cfg(test)]
mod policy_tests;
#[cfg(test)]
mod split_tests;
