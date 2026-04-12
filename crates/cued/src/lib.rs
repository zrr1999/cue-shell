//! cued — background daemon for cue-shell.
//!
//! This crate contains the parser, actor system, and process manager.

pub mod actor;
pub mod dirs;
pub mod parser;
pub mod pty;
pub mod ring_buffer;
pub mod storage;
