# Changelog

## 3.6.0 (Rust) / 2.9.0 (Python), 2026-11-19

- **Data exports are searchable.** Two new source kinds read the official exports of chats held in the cloud: `chatgpt_export` and `claude_export`. Point either at a folder with an extracted export.
  - Each conversation is a session. A hit's `line` numbers its message, `record_sha256` hashes the message object, and `read` checks it.
  - ChatGPT branches are read depth-first, and `current_branch` marks the branch the app shows.
  - Hidden and system messages are `injected_context`; tool calls and results are `tool_input` and `tool_output`; text extracted from attachments is `quoted_or_pasted`. Reasoning and thinking are never searched or returned.
  - Each file's newest conversation update is a coverage note. A ZIP is reported, not opened, and unknown content counts as unsupported in `find` and `read`.
  - The same message in a second export of the same conversation is listed under `copies`, with its offset and branch, not counted again.
  - Limits: files over 1 GiB, nesting deeper than 256 levels, duplicate conversation ids within a file, repeated message ids within a conversation, and conversations holding a U+10F800..U+10FFFF character are reported, not read.
- **Never default.** Neither kind is added to the default sources.
- **Cursors** from 3.5.1 and 2.8.1 are rejected, as after every version change.

## 3.5.1 (Rust) / 2.8.1 (Python), 2026-11-19

- **Machine-written text is no longer labelled as the user's.** Claude Code writes some text into user-role records itself. That text now gives `injected_context`, not `user_message`, and ranks below what the user typed. It opens with one of eight tags:
  - `<task-notification>`, `<local-command-caveat>` and `<local-command-stdout>`;
  - `<cross-session-message>`, `<ci-monitor-event>` and `<scheduled-task>`;
  - `<fork-boilerplate>` and `<artifact-view-context>`.
- **Unchanged:** the user's own pastes, slash commands and reply quotes keep `user_message`.

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
