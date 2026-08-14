//! Shared helpers for recap integration tests.
//!
//! Every test binary compiles this whole tree but uses only the part it needs,
//! so the unused halves are allowed rather than reported as dead code.

#[allow(dead_code)]
pub mod redis_fixture;
#[allow(dead_code)]
pub mod sqlite_fixture;
