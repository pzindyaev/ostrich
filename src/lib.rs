//! Ostrich: a terminal dashboard for QEMU virtual machines.
//!
//! The crate is split the way the program is: [`config`] is the app's own
//! settings, [`vm`] is everything that touches QEMU, disks, firmware and the
//! host, and [`tui`] is the ratatui front end on top of it.

pub mod config;
pub mod tui;
pub mod vm;

/// Version string baked in at build time (see build.rs).
pub const VERSION: &str = env!("OSTRICH_VERSION");
/// Short commit hash baked in at build time.
pub const COMMIT: &str = env!("OSTRICH_COMMIT");
/// Build or commit date baked in at build time.
pub const BUILD_DATE: &str = env!("OSTRICH_DATE");
