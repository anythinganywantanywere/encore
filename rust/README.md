# Encore daemon, in Rust

This is a work-in-progress rewrite of the Encore Tweaks daemon (`encored`)
from C++ to Rust. The original project is Encore Tweaks 5.2.2 by
Rem01Gaming; this directory only contains the Rust side of it. Nothing
here changes the WebUI, the installer scripts, the resolver APK, or the
config and gamelist formats — the goal is the same behaviour, in a
safer language.

The C++ daemon in `../jni/` is still the shipped one. The Rust daemon
is a test build until device parity is actually proven, not just claimed.

## Status

What works so far:

- The full daemon command set: `daemon`, `setup_gamelist`,
  `check_gamelist`, `version`.
- Gamelist and config handling, `module.prop` parsing, logging with
  rotation, inotify watching, single-instance locking, and profile
  planning per SoC (MediaTek, Snapdragon, Exynos, Unisoc, Tensor, Tegra,
  plus a common-only fallback for Kirin and unknown SoCs).
- Binder support: transaction codes are resolved at runtime through the
  Java resolver APK, the five system services are acquired through
  `libbinder_ndk.so`, and the daemon reacts to foreground, process-died,
  display, and power-save events. DND is applied through
  `cmd notification set_dnd`.
- Android builds for `arm64-v8a` and `armeabi-v7a` (platform 29), and a
  flashable test module built by swapping only the `encored` binaries
  into the original module zip.

Tested on a real device, 28 October 2026: Redmi Note 5 Pro (whyred,
Snapdragon 636, LineageOS Android 13, KernelSU). The v4 test module
starts, initialises Binder, detects Snapdragon, applies perfcommon at
startup (155 sysfs writes applied, 14 missing, 0 rejected, 0 failed),
then settles on balance while idle (51 applied, 6 missing, 0 rejected,
0 failed). The WebUI shows the daemon running with the balance profile.

Not done yet, honestly:

- Game foreground on a device (performance profile + `gameinfo`), game
  exit, screen-off, battery saver, and DND during play have not been
  tested on a device yet.
- No test on a non-Qualcomm device, and no Magisk or APatch install test.
  The module format is the same for all three, but only KernelSU has
  actually been flashed so far.
- `notify` is missing, so fatal-error notifications and the module
  description update don't happen yet. SoC and model detection through
  `getprop` is also missing; a device whose SoC is only visible through
  properties falls back to the common-only plan.
- The C++ code has not been removed, and the upstream packaging scripts
  have not been switched over. That only happens after parity is proven.

The full verification notes, including the quirks copied from the C++
behaviour and the things still marked `TODO(verify)`, live in
`VERIFICATION.md`. That file is the source of truth for what was checked
against the C++ code and what was not.

## Layout

- `crates/encore_core/` — the daemon logic: profiles, gamelist, config,
  registry, logging, watcher, profiler, sysfs allowlist, mitigation
  rules, Binder state machine, resolver protocol, and the transport
  client. No `unsafe` code here.
- `crates/encore_sys/` — the one exception to that rule: a thin crate
  with the `fork`/`sigaction` wrappers and the `libbinder_ndk.so`
  loader. Every `unsafe` block in it has a `SAFETY` comment explaining
  why it is sound.
- `crates/encored/` — the binary itself. It parses the same commands as
  the C++ daemon and requires root, same as the original.
- `deny.toml` — the licence allow list for `cargo deny`, kept minimal
  for what is actually in `Cargo.lock`.

Two things worth knowing about the design:

- Sysfs writes only go through a static allowlist in one module, and
  paths containing `..` are rejected. A single failed write never kills
  the daemon.
- Important files (`gameinfo`, `current_profile`, config, `module.prop`)
  are written atomically: temp file in the same directory, fsync,
  rename, explicit permissions. The C++ daemon truncated them in place.

## Building

Host build, for development and tests:

```bash
cd rust
cargo test --all
```

Android build, for a device:

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi
cargo install cargo-ndk
ANDROID_NDK_HOME=/path/to/android-ndk-r27d \
  cargo ndk -t arm64-v8a -t armeabi-v7a --platform 29 \
  build --release -p encored
```

The binaries end up in
`target/aarch64-linux-android/release/encored` and
`target/armv7-linux-androideabi/release/encored`. Copy them into
`libs/arm64-v8a/` and `libs/armeabi-v7a/` of a module zip, regenerate
the `.sha256` files, and re-zip. The test module also changes one line
in `service.sh` to `encored daemon --daemonize`, because the Rust daemon
only detaches with that flag — the C++ daemon detached on its own.

## Checks

These all have to pass before a change lands:

```bash
cd rust
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
cargo deny check
cargo audit
```

`cargo deny` and `cargo audit` need those tools installed; CI installs
them (see `../.github/workflows/rust.yml`). If a gate cannot run in
some environment, report it as not run, not as passed.

For host testing only, `encored` honours `ENCORE_CONFIG_DIR`,
`ENCORE_MODPATH`, and `ENCORE_DATA_DIR` to point at a fake config,
module, and data directory. Without them it uses the real device paths
(`/data/adb/.config/encore`, `/data/adb/modules/encore`).

## Licence

Apache-2.0, same as upstream. See `../LICENSE` and `../NOTICE.md`.
