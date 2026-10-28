<p align="center"><a href="https://encore.rem01gaming.dev" target="_blank"><img src="https://encore.rem01gaming.dev/logo.webp" width="200" alt="Encore Tweaks Logo"></a></p>

<p align="center">
<a href="https://t.me/rem01schannel"><img src="https://img.shields.io/badge/Follow-Telegram-white.svg?style=for-the-badge&logo=telegram&logoColor=white&labelColor=222" alt="Join Our Telegram Channel"></a>
<a href="https://encore.rem01gaming.dev/download"><img src="https://img.shields.io/github/downloads/rem01gaming/encore/total?style=for-the-badge&logoColor=white&labelColor=222" alt="Total Downloads"></a>
<a href="https://github.com/Rem01Gaming/encore/releases"><img src="https://img.shields.io/github/v/release/rem01gaming/encore?label=Release&style=for-the-badge&logo=github&logoColor=white&labelColor=222" alt="Latest Stable Version"></a>
<a href="https://encore.rem01gaming.dev"><img src="https://img.shields.io/badge/Apache-white?style=for-the-badge&logo=andela&logoColor=white&labelColor=222" alt="License"></a>
</p>

## About this fork

This is an unofficial, work-in-progress fork. The original project is
Encore Tweaks by Rem01Gaming, described below. Nothing here is an
official release, and nothing here replaces the upstream project.

What this fork does, in short:

- **Rewrites the `encored` daemon from C++ to Rust**, in the `rust/`
  directory. The goal is the same behaviour in a safer language:
  the daemon runs as root, so memory safety, atomic file writes,
  single-instance locking, and bounded external commands matter more
  than binary size. The C++ daemon in `jni/` is still in the tree;
  the Rust daemon is the test build until device parity is proven.
- **Integrates Frame Aware Scheduling (FAS) at Level 2.** When a
  gamelist game runs and the user has enabled FAS in the app, the
  Rust daemon attaches `fas_ctl` to the game process automatically
  and detaches on game exit. FAS can be turned on and off from the
  Encore app itself (Settings > Frame Aware Scheduling), without a
  reboot: the daemon watches `config.json` and applies the change
  on the next loop iteration. The setting is
  `preferences.enable_fas` in `config.json`, default off, so
  existing users see no behaviour change until they enable it.
  The C++ daemon ignores that unknown JSON field.
- **Changes nothing else about the format.** WebUI (except the new
  FAS settings page), installer scripts (except FAS extracts and the
  kernel-module load guard), resolver APK, gamelist format, and
  config schema stay compatible with upstream.

### Status, 28 October 2026 — tested on a real device

Test device: Redmi Note 5 Pro (whyred, Snapdragon 636, LineageOS
Android 13, KernelSU).

Proven on that device, in a single daemon run:

- Daemon starts, initialises Binder, detects Snapdragon, applies
  perfcommon at startup (155 sysfs writes applied, 14 missing,
  0 rejected, 0 failed), then settles on balance while idle
  (51 applied, 6 missing, 0 rejected, 0 failed).
- Game foreground applies performance (52 applied, 6 missing),
  game exit returns to balance, repeatedly.
- Battery Saver on applies powersave (64 applied, 6 missing),
  Battery Saver off returns to balance.
- Screen-off for about 2 minutes: balance during lock, performance
  on unlock, FAS silent during lock.
- FAS toggle at runtime, both directions, without reboot or daemon
  restart: off stops all attach attempts, on resumes them.
- DND for a DND-enabled game: `zen_mode` 1 (priority) during play,
  0 after exit, with profile 2 on exit.
- Config reload and gamelist reload work on device (WebUI changes
  are picked up without reboot).
- FAS failure handling: with no kernel module loaded, attach fails
  gracefully with a warning only, never failing a profile. After
  the 30-second retry backoff fix, warnings are at most one per
  30 seconds per game PID, down from 537 warnings in 66 seconds
  in the earlier build.

Full evidence, with timestamps and log excerpts, is in
[rust/VERIFICATION.md](rust/VERIFICATION.md) (§22–§26). The Rust
workspace, its quality gates (fmt, clippy with warnings denied,
tests, deny, audit), and how to build it are documented in
[rust/README.md](rust/README.md).

### Honest limitations — read before flashing

- **This is a test build, not a stable release.** Do not treat it
  as official Encore Tweaks.
- **FAS does not measure frames yet on a real phone.** That needs
  a kernel module (`.ko`) built for the target kernel. A `.ko` is
  kernel-specific: the QEMU v6.12 `.ko` cannot be used on whyred
  4.19, and no whyred `.ko` has been built yet. Until one exists,
  Encore works fully, the toggle exists, `fas_ctl` exists, but
  attach attempts fail with a warning only. `service.sh` tries
  to load `system/lib/modules/encore_fas.ko` if present and
  continues with Encore only if loading fails.
- **Only KernelSU on whyred has actually been flashed.** Magisk
  and APatch use the same module format, but their installs have
  not been tested. No non-Qualcomm device has been tested.
- **Still missing in the Rust daemon:** `notify` (the uid/gid-2000
  drop before posting a notification), so fatal-error
  notifications and module-description updates do not happen yet;
  SoC and model detection through `getprop` (sysfs detection
  covers the common cases, anything else falls back to the
  common-only plan).
- **Flash only the module zip, never a source zip.** Source zips
  (the tree with `rust/` in it) contain no compiled `libs/`
  binary and their `META-INF/` is nested under `module/`, so
  KernelSU/Magisk/APatch will reject them. Rollback is just
  uninstalling the test module and rebooting; the original C++
  module zip can be flashed again afterwards.

### What changed in this fork, file by file

- `rust/` — new Rust workspace (`encore_core`, `encored`,
  `encore_sys`), plus `rust/VERIFICATION.md`.
- `.github/workflows/rust.yml` — new Rust CI.
- `webui/src/views/FasToggle.vue`, `webui/src/router/index.js`,
  `webui/src/stores/EncoreConfig.js`,
  `webui/src/views/Settings.vue`,
  `webui/src/locales/strings/en.json`,
  `webui/src/locales/strings/id.json` — FAS settings page, route,
  store, Settings entry, and English/Indonesian strings (other
  locales fall back to English).
- `module/customize.sh` — guarded extracts for `fas_ctl`
  (per-arch, from `libs/`) and for `encore_fas.ko` when present,
  plus the KSU/APatch `fas_ctl` symlink with the staging-path
  guard fix.
- `module/service.sh` — guarded `insmod` of `encore_fas.ko`
  with a fallback log, before starting the daemon.
- `.gitignore` — one line for `rust/target/`.

The FAS kernel module itself lives in a separate project,
[rem01project/encore_fas](https://github.com/rem01project/encore_fas)
(Frame Aware Scheduling, GPL-2.0-only). Its C detector is the
default; its experimental Rust detector (Phase 4b) is off by
default and is not shipped in the test module. Do not copy that
project's directory into this repo when pushing: the two repos
stay separate.

For fork-specific issues, use this fork's Issues tab. For
upstream Encore Tweaks, use the upstream links below.

## About Encore Tweaks

**Encore Tweaks** is a dynamic Magisk module designed to maximize your device's gaming performance while intelligently preserving battery life during everyday use. Perfect for gamers who demand smooth gameplay without compromising daily usability!

More info about this project on [Official Encore Tweaks website](https://encore.rem01gaming.dev/)

## Supported Root Managers

- [APatch](https://github.com/bmax121/APatch)
- [KernelSU](https://github.com/tiann/KernelSU)
- [Magisk](https://github.com/topjohnwu/Magisk)  <sup>([no WebUI](https://github.com/topjohnwu/Magisk/issues/8609#event-15568590949)👀)</sup>

### Also Supported on

- [KsuWebUI](https://github.com/5ec1cff/KsuWebUIStandalone)   <sup>🌐</sup>
- [WebUI-X](https://github.com/MMRLApp/WebUI-X-Portable)   <sup>🌐</sup>
- [MMRL](https://github.com/MMRLApp/MMRL)

## Resources

- [FAQ](https://encore.rem01gaming.dev/guide/faq.html) - Common questions answered
- [Configuration Guide](https://encore.rem01gaming.dev/guide/webui-and-configuration.html) - How to configure the WebUI
- [Download](https://encore.rem01gaming.dev/download) - Get the latest version

## Community & Support

- [Telegram Channel](https://t.me/rem01schannel) - Stay updated with announcements
- [Report Issues](https://github.com/rem01gaming/encore/issues) - Found a bug? Let us know!
- [Contribute](https://github.com/rem01gaming/encore/pulls) - Submit improvements via PR

## Translation

Interested in translating Encore Tweaks? We use Weblate to crowdsource translations, allowing the community to collaborate on making the project accessible in multiple languages. Visit our translation project [here](https://hosted.weblate.org/projects/encore-tweaks/) and help us reach more users worldwide!

<a href="https://hosted.weblate.org/widget/encore-tweaks/webui/horizontal-auto.svg" alt="Weblate translation status"></a>

## License

Encore Tweaks is open-sourced software licensed under the [Apache-2.0 license](https://www.apache.org/licenses/LICENSE-2.0).
