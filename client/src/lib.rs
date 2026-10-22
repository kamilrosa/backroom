//! Backroom client as a library — `main.rs` only wires things together,
//! while tests and tools can use individual modules.

pub mod audio;
pub mod config;
pub mod dsp;
pub mod hotkey;
pub mod jitter;
pub mod mic;
pub mod net;
pub mod state;
pub mod ui;
