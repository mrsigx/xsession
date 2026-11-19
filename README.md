# xsession

Search the local transcripts of your AI coding agents — Claude Code, OpenAI Codex (including its zstd-compressed rollouts) and Google Antigravity — from one read-only command line, and get back hits you can cite: file, line, byte offset, the SHA-256 of the raw record, who said it, and how sure the tool is about that.

There are two implementations with the same commands, JSON output and exit codes:

- **Rust** (`src/`, version 3.6.0): fast and multithreaded.
- **Python** (`python/xsession.py`, version 2.9.0): standard library only, for when you can't build the binary.

The JSON field `version` tells you which one answered.

## What it is for

Several agents across several sessions leave several transcript stores. When you need to know what was actually said, by whom and where, a guess or a summary isn't enough. You want the exact record. xsession finds it, labels it (a user message is not the same as a tool's output or a pasted quote), and lets you reopen it later by line and digest, so the evidence can be checked.

It is on demand only:

- It runs no daemon or service, builds no index, and calls no model.
- It reads only the folders you configure, which may include network or WSL paths if you point it at them.
- It writes nothing, unless you opt into paging with `--snapshot-dir`.

## Build

```sh
cargo build --release          # target/release/xsession (xsession.exe on Windows)
```

Or run the Python core directly. It needs Python 3.10+, and 3.14+ to read `.jsonl.zst`; on older versions those files are reported, not skipped:

```sh
python python/xsession.py <command> ...
```

## Quick start

```sh
xsession sources --json                                    # what will be searched, and what is missing
xsession find "the phrase you remember" --limit 10 --json  # literal, case-folded search over decoded records
xsession resolve <full-session-id> --json                  # every physical file behind one conversation
xsession read <full-session-id> --line 42 --record-sha256 <digest> --context 1 --json
xsession list 15 --json                                    # recent sessions, newest first
xsession path <full-session-id>                            # one transcript path, or a refusal
xsession current                                           # this agent's own session, if it can tell
```

Notes on input:

- **Queries are literal.** They are not regular expressions and not shell syntax. A query that begins with a hyphen goes after `--`: `xsession find --json -- --no-verify`.
- **IDs must be full, or a unique prefix.** An unknown ID never falls back to "the newest session". An ambiguous prefix lists the candidates and exits 3.

## Where it looks

| Source name | Kind | Default path | Override |
|---|---|---|---|
| `claude` | `claude` | `~/.claude/projects` | `CLAUDE_PROJECTS_DIR` |
| `codex-windows` | `codex` | `~/.codex/sessions` | `CODEX_WIN_SESSIONS_DIR` |
| `codex-archive` | `codex` | `archived_sessions` next to the above | (follows the above) |
| `chatgpt-projects` | `chatgpt_project` | `~/.codex/.chatgpt-projects` | `CHATGPT_PROJECTS_DIR` |
| `antigravity` | `antigravity` | `~/.gemini/antigravity/brain` | `ANTIGRAVITY_BRAIN_DIR` |
| `codex-wsl`, `codex-wsl-archive` | `codex` | `~/.codex/sessions` when not on Windows | `CODEX_SESSIONS_DIR` |

Details for this table:

- **`XSESSION_WINDOWS_HOME`** changes the home that the first five defaults sit under. Under WSL, set it to `/mnt/c/Users/<you>` to read the stores of agents that run on Windows; the WSL pair then reads your Linux-side Codex history.
- **Outside Windows,** if `XSESSION_WINDOWS_HOME` is unset, the WSL pair would name the same folder as `codex-windows`, so it is left out. A Linux or macOS user's own `~/.codex` history therefore appears under the historical names `codex-windows` and `codex-archive`.
- **On Windows,** WSL history is opt-in through `CODEX_SESSIONS_DIR`, and xsession never starts a distro.
- **Missing sources are reported,** not treated as empty.

For a custom set, pass a JSON file with `--sources <file>` (or set `XSESSION_CONFIG`). Use `--source <name>` to narrow any set:

```json
{
  "sources": [
    {"name": "claude", "kind": "claude", "path": "/home/me/.claude/projects"},
    {"name": "archive", "kind": "codex", "path": "/home/me/.codex/archived_sessions"},
    {"name": "notes", "kind": "document", "path": "/home/me/project/docs"}
  ]
}
```

A `document` source searches `.md`, `.txt`, `.json` and `.jsonl` files line by line, as documents. Their lines are not attributed to any speaker.

### Data exports from ChatGPT and claude.ai

Chats you had in ChatGPT or claude.ai are not files on your computer, but both services let you export them as a ZIP. Extract the ZIP and add its folder as a source. These kinds are never default sources:

```json
{"name": "chatgpt-export", "kind": "chatgpt_export", "path": "/home/me/exports/chatgpt"},
{"name": "claude-export", "kind": "claude_export", "path": "/home/me/exports/claude"}
```

How an export is read:

- **Files.** Every `conversations.json` or `conversations-NNN.json` under the folder is read. A ZIP is reported in `coverage.notes`, not opened.
- **Sessions.** Each conversation is a session, with the export's own conversation id, title and creation time. `export_offset` and `bytes` give its span in the file, and the span's SHA-256 guards every later read of it. A conversation id seen earlier in the same file is reported as `identity_duplicate`, not listed twice.
- **Hits.** `line` numbers the message within its conversation, in reading order, and `block` names the field. An export is one JSON document, so `byte_offset` is null, and `record_sha256` hashes the whole message object as Python's `json.dumps` writes it. `read <id> --line N --record-sha256 <digest>` still refuses a changed message.
- **Branches.** ChatGPT keeps a conversation as a tree, with edits and regenerations as branches. It is read depth-first, and `current_branch` says whether a message is on the branch the app shows.
- **Roles.** User messages the app hides (custom instructions and context it adds) and system messages are `injected_context`. An assistant message addressed to a tool is `tool_input`, and a tool's reply is `tool_output`. Text claude.ai extracted from an attached file is `quoted_or_pasted`.
- **Never searched or returned:** ChatGPT thoughts and reasoning recaps, and claude.ai thinking. They are parsed with the rest of the file, and they are part of the message `record_sha256` hashes.
- **Age.** An export is a snapshot. Each file's newest conversation update is a coverage note, so you can see how old it is.
- **Unknown shapes.** In `find` and `read`, an unknown content type, part, block type, tool result item, attachment, sender or mapping node, and a second message with a message id already seen in its conversation, are counted as unsupported records, so coverage reads partial instead of looking complete. `list` skips them without counting, as it does for transcripts.
- **Copies.** The same conversation in two exports is two sessions. In `find`, the same message (by its id, or its line when it has none) and field found again in the other export is listed under `copies` on the first hit, with its `export_offset` and `current_branch`, not as a second hit. Two identical messages without ids in one conversation stay two hits; a message id seen twice in one conversation is the unsupported record above.
- **Limits.** A file over 1 GiB is reported as `export_too_large`, and a file nesting arrays and objects more than 256 levels deep as `export_unsupported`. A conversation holding a character in U+10F800..U+10FFFF, literal or escaped as a surrogate pair, is an unsupported record in both implementations: the Rust core cannot tell it from the placeholder it uses for a lone surrogate. The check is conservative: any `\udbfe` or `\udbff` escape in the conversation's text counts, even one that is unpaired or follows an escaped backslash.

Tested against real ChatGPT and claude.ai exports. The test suite builds its own synthetic exports.

## What a hit tells you

Each match carries:

- the source, the full conversation identity, and the physical path;
- the one-based line, the byte offset and the block;
- the raw-record SHA-256 and the timestamp, when the record has one;
- the speaker role and a provenance class;
- a bounded excerpt, explicitly marked when truncated.

Provenance classes keep different kinds of text apart:

| Class | Meaning |
|---|---|
| `user_message` | The stored record has the user role. Its `role_only` confidence is not proof that a person typed every word. |
| `quoted_or_pasted`, `injected_context` | Text that looks pasted, text the app wrapped around the user's input, text Claude Code wrote into a user record itself (a task notification, a local command's output, a message from another session), or a mid-turn message that no person sent |
| `tool_input`, `tool_output` | Tool arguments (decoded, with their field path in `block`) and tool results |
| `delegated_prompt` | A prompt one agent wrote to a sub-agent |
| `compaction_summary`, `resumed_history` | Summaries, and history copied into a resumed session |
| `project_instructions`, `project_source` | ChatGPT project files, checked against their manifest |

What isn't searched:

- **Only documented text fields are indexed:** message text, tool inputs and tool results. Claude `thinking` blocks, Codex `reasoning` items and Antigravity `thinking` are never indexed or printed. Binary attachments and unknown record types aren't searched either.
- **Claude Code messages sent mid-turn are searched.** A message that arrives while Claude Code is working is stored as an `attachment` record of type `queued_command`, not as a user record. One whose sender is recorded as human is a `user_message`. Any other sender, such as a task notification, another session or a monitor event, is `injected_context`.
- **Claude Code records that are not searched:** `queue-operation` records, which are queue bookkeeping that repeats a message; every other attachment type, such as reminders, hook output and file or skill listings; and `system` and `api-request` records. Every response names these under `coverage.excluded`. A queued text that never became a message exists only in the queue records, so it can't be found.
- **Copies** of a record are labelled as copies. They never count as independent corroboration.

## Coverage, paging and limits

Every response states the coverage of the configured sources: what was excluded, which roots were inaccessible, and any read, parse or change errors.

**Defaults:**
- `find` returns 20 results (at most 1,000).
- `list` shows 15 sessions.
- At most 10,000 matches are kept (`--max-matches` 1..100,000). An over-broad query fails with exit 2 rather than looking like "no match".
- The JSON envelope is capped at 256 KiB (`--max-output-bytes`).
- Excerpts default to 800 characters.

**Paging is opt-in:**
- `--snapshot-dir <dir>` (or `XSESSION_SNAPSHOT_DIR`) stores bounded snapshots: at most 16 files of 4 MiB each in that folder.
- `--cursor` continues the same captured set of files and byte prefixes.
- Changed evidence rejects the cursor.

**Large scans** (256 MiB and up) run on `--jobs` workers (1..64; `XSESSION_JOBS`): threads in Rust, processes in Python. The default is the CPU count, up to 8. Output is identical to a serial scan.

**`codex://threads/...` URLs** exit with code 5 and `native_required`. Those conversations live in the app, not on disk.

| Exit | Meaning |
|---|---|
| 0 | Results with complete configured coverage |
| 1 | No match within complete coverage, or no reliable current identity |
| 2 | Usage, configuration or output-budget error |
| 3 | Ambiguous identity, multiple segments, stale cursor, or a changed record |
| 4 | Partial coverage: results may be present; read the diagnostics |
| 5 | Native app retrieval required |
| 6 | Operational failure |

## Tests

```sh
cargo test --release
cd conformance
python -I -B -m unittest discover -s . -p "test_*.py"                  # the Python core
XSESSION_BINARY=../target/release/xsession python -I -B -m unittest discover -s . -p "test_*.py"   # the same suite on the binary
```

In PowerShell, set the binary first with `$env:XSESSION_BINARY = (Resolve-Path ..\target\release\xsession.exe)`.

The Rust unit tests check the port against Python-generated oracle vectors in `conformance/vectors/`, which `tools/gen_vectors.py` regenerates. `tools/gen_tables.py` regenerates the Unicode tables in `src/tables.rs` from Python's `unicodedata`. The conformance suite drives either implementation through the CLI only, with synthetic fixtures built at run time.

## Status and known limits

- **Platforms.** Built and tested on Windows 11 with the MSVC toolchain. Linux and macOS builds are untested in this release. The conformance suite assumes Windows in places: one fixture creates a junction with `mklink /J`.
- **Antigravity truncation.** Antigravity's `transcript.jsonl` keeps only about the first and last 2 KiB of a long field, so the middle of those fields can't be found. `transcript_full.jsonl` is not searched.
- **`current`** knows Claude Code and Codex session IDs (`CLAUDE_CODE_SESSION_ID`, `CODEX_THREAD_ID`), but not Antigravity's.
- **Sub-agent detection.** A sub-agent whose creating conversation is missing from the inventory can't be recognised as a sub-agent.

## Also from Null Limit

- [skills](https://github.com/mrsigx/skills): free-to-use agent skills, including number-play and aim-profile.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Third-party pieces keep their own licenses:

- the crates `serde_json`, `miniz_oxide` and `zstd`, which bundles libzstd;
- the Unicode data behind `src/tables.rs`, derived from the Unicode Character Database through Python's `unicodedata` under the Unicode License v3.
