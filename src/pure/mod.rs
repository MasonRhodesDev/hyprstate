//! Layer 3: pure transition maps and policy functions.
//!
//! Nothing in this module performs I/O — no `std::fs`, no subprocess, no
//! D-Bus, no clocks (functions that need time take `Instant` parameters).
//! Everything arrives as plain values and the unit tests (the port of
//! test_hyprstate.py) live against exactly this surface.

pub mod fsm;
pub mod gpu;
pub mod power;
pub mod profiles;
