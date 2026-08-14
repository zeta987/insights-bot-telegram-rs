//! Redis-backed recap state.
//!
//! [`keys`] pins every key literal and hash codec to Go v1.0.0 commit
//! `02aee8ce260165592e2152eb5a024a602e4eced1`; [`recap_state`] holds the
//! [`recap_state::RecapStateStore`] abstraction together with its production
//! Redis backend and its deterministic in-memory double.
//!
//! The automatic TimeCapsule scheduling queue (`REDIS-001`) is deliberately
//! absent here and lands in Task 13, so handler state stays reviewable on its
//! own.

pub mod keys;
pub mod recap_state;
