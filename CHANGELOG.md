# Changelog

## 3.5.0 (Rust) / 2.8.0 (Python), 2026-11-19

- **Mid-turn messages are found.** A message sent to Claude Code while it was in the middle of a turn is stored as an `attachment` record of type `queued_command`, not as a user record. Earlier versions skipped these records and still reported complete coverage.
  - A sender recorded as human gives `user_message`. Any other sender gives `injected_context`.
  - A copy inherited by a resumed or forked session is `resumed_history`, like any other inherited record.
- **Disclosure.** When a Claude source is configured, `coverage.excluded` names the Claude record types that are not searched: queue bookkeeping, other attachment types, and `system` and `api-request` records.
- **Cursors** from 3.4 and 2.7 are rejected, as after every version change.

## 3.4.0 (Rust) / 2.7.0 (Python), 2026-11-03

First public release.

- **Default sources.** Outside Windows, the default sources now sit under your own home, not under a fixed Windows user folder.
  - `XSESSION_WINDOWS_HOME` points them elsewhere. Under WSL, set it to `/mnt/c/Users/<you>`.
  - Without it, the WSL Codex pair is skipped when it would name the same folder as `codex-windows`.
- **Licensing.** Licensed under `MIT OR Apache-2.0`.

Earlier versions (Rust 3.0 to 3.3, Python 2.0 to 2.6) were developed and used privately.
