//! # android-backup-core
//!
//! Backup engine, ADB integration, manifest storage and configuration for the
//! Android Backup Utility.
//!
//! ## Architectural invariant
//!
//! This crate **must never depend on a UI crate** (`eframe`, `egui`, `rfd`, …).
//! The desktop UI subscribes to events emitted here and forwards user intents
//! back through [`backup::controller`]. This keeps a CLI, a background service
//! or a different GUI possible without touching the engine (see `docs/overview_plan.md` §3).
//!
//! ## Error convention
//!
//! Every module exposes its own `thiserror` enum; [`Error`] is the thin
//! top-level enum that delegates to them. `unwrap`/`expect`/`panic` are denied by
//! lint in non-test code so that every failure path is typed and reportable.
//!
//! ## Layering
//!
//! ```text
//! paths / clock            pure helpers, no I/O
//! config                   serde config file
//! database                 SQLite manifest (devices, folders, jobs, files)
//! adb                      ADB discovery, process runner, output parsers
//! devices                  device identity, state mapping, polling watcher
//! backup                   selection, enumeration, comparison, transfer, jobs
//! ```
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![forbid(unsafe_code)]

pub mod adb;
pub mod backup;
pub mod clock;
pub mod config;
pub mod database;
pub mod devices;
pub mod error;
pub mod mock;
pub mod paths;
pub mod testutil;

pub use error::{Error, Result};
