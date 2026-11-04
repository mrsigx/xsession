# Changelog

## 3.4.0 (Rust) / 2.7.0 (Python), 2026-11-03

First public release.

- **Default sources.** Outside Windows, the default sources now sit under your own home, not under a fixed Windows user folder.
  - `XSESSION_WINDOWS_HOME` points them elsewhere. Under WSL, set it to `/mnt/c/Users/<you>`.
  - Without it, the WSL Codex pair is skipped when it would name the same folder as `codex-windows`.
- **Licensing.** Licensed under `MIT OR Apache-2.0`.

Earlier versions (Rust 3.0 to 3.3, Python 2.0 to 2.6) were developed and used privately.
