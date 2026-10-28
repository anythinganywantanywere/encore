#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Core logic for the Encore Tweaks Rust migration.
//!
//! This crate contains no network access. The Binder
//! transport itself lives in `encore_sys::binder` (the
//! `libbinder_ndk.so` loader, service acquisition,
//! single-value transactions, and callback
//! registration) plus [`binder_client`] here (the query
//! client, callback mapping, and change-only power-save
//! polling); the transport is not yet wired into the
//! `encored` daemon loop, because performing profile
//! applications on a host would be wrong and the loop
//! wiring is device work (see `rust/VERIFICATION.md`
//! §16). Everything around that transport that can be
//! verified on a host is implemented: the resolver
//! protocol, the device-mitigation rules, the
//! Binder-driven state machine in [`daemon_state`], the
//! profiler plans, the sysfs allowlist, the inotify
//! watcher, and the file-backed stores, with atomic writes
//! where the C++ daemon truncated files in place. Each
//! module mirrors one verified behaviour of the C++ daemon
//! (see `rust/VERIFICATION.md`), split so the pure
//! transformation can be tested without a device.
//!
//! Stage 2 scope: leaf modules (game registry, config store,
//! `module.prop`) wired to the `encored` commands `setup_gamelist`
//! and `check_gamelist`. The daemon command itself is still not
//! implemented, and the C++ daemon remains the shipped daemon.
//! Nothing in this crate is wired into `service.sh` or the module
//! installer yet.
//!
//! Stage 3 scope: logging and file utilities. The logger in
//! [`log`] and the helpers in [`file_util`] are wired into every
//! `encored` command, with a size limit and rotation that C++ did
//! not have. The daemon command itself is still not implemented.
//!
//! Stage 6 scope: the Binder-driven state machine. A
//! `DaemonState` consumes decoded Binder events and
//! returns a `ProfileApplication` (gameinfo content,
//! profile code, profiler plan, DND action) for the caller
//! to perform, in that order, without holding a lock
//! across profile application. The daemon loop in
//! `encored` does not yet receive Binder events, because
//! there is no Binder transport on a host; when a
//! transport exists, it feeds events to this machine and
//! performs the returned applications.

/// Atomic file writing, as required for important files such as
/// `module.prop` and config files.
pub mod atomic_write;
/// Binder transport for the daemon loop, built on
/// `encore_sys::binder`: query client, callback mapping,
/// and change-only power-save polling.
pub mod binder_client;
/// Configuration parsing and rendering, mirroring `EncoreConfigStore`.
pub mod config;
/// File-backed configuration store, mirroring `EncoreConfigStore`.
pub mod config_store;
/// Binder-driven daemon state machine, mirroring `Main.cpp`.
pub mod daemon_state;
/// Daemonising, mirroring C++ `daemon(0, 0)`.
pub mod daemonize;
/// Do Not Disturb execution, mirroring `MiscUtility.cpp`.
pub mod dnd;
/// File utilities, mirroring the small C++ helpers.
pub mod file_util;
/// Game list parsing, mirroring `GameRegistry::load_from_json`.
pub mod gamelist;
/// Logging, mirroring `EncoreLog`.
pub mod log;
/// Device-mitigation rules, mirroring `DeviceMitigationStore`.
pub mod mitigation;
/// `module.prop` parsing and merging, mirroring `ModuleProperty`.
pub mod module_prop;
/// File-backed `module.prop` store, mirroring `ModuleProperty`.
pub mod module_prop_store;
/// Strict package-name validation for untrusted input.
pub mod package_name;
/// PID tracking, mirroring the C++ process helpers.
pub mod process;
/// A subprocess runner with a timeout and output cap.
pub mod process_runner;
/// Profile modes, mirroring `EncoreProfileMode` in `jni/include/Encore.hpp`.
pub mod profile;
/// Profiler plans as data tables, mirroring `encore_profiler.sh`.
pub mod profiler;
/// Snapshot game registry, mirroring `GameRegistry`.
pub mod registry;
/// Binder transaction-code resolver protocol.
pub mod resolver;
/// Running the Binder resolver with a timeout and cap.
pub mod resolver_runner;
/// Graceful SIGTERM/SIGINT handling via a self-pipe.
pub mod signals;
/// Sysfs/procfs writing under a static allowlist.
pub mod sysfs;
/// The Binder event-source seam for the daemon loop.
pub mod transport;
/// Inotify file watching, mirroring `InotifyWatcher`.
pub mod watcher;
