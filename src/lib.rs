//! gloss — out-of-tree git checkpoints for AI coding agents.
//!
//! See `README.md` and `PLAN.md` at the crate root for the design and
//! build plan. This crate's public surface is the `checkpoint` module:
//! everything else (CLI, daemon, web UI, MCP) is built on top of it.

pub mod checkpoint;
pub mod install;
pub mod mcp;
pub mod web;

pub use checkpoint::{
    diff, gc_stale_refs, list, restore, snapshot, CheckpointInfo, FileDiff, FileStatus,
    RestoreReport, DEFAULT_GC_RETENTION_DAYS,
};
