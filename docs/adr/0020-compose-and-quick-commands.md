# ADR-0020: Reviewed Compose and saved command templates

- Status: Accepted
- Date: 2026-10-01

## Compose input

P2-030 adds a logical `InputAction::Compose { text, append_enter }`. Each destination daemon encodes the body using its own bracketed-paste mode and places the optional Enter after the closing paste marker. Enter also uses the destination keyboard protocol, including a release event when requested by Kitty. One request owns both parts so the desktop cannot enqueue the body and then lose a separate Enter to backpressure. This is an atomic queue action, with no claim of remote execution, transactional shell effects or rollback.

IPC minor 22 introduces COMPOSE_INPUT at bit 26. Desktop clients require the negotiated capability and old daemons reject an unnegotiated Compose action with a framed control response. There is no fallback to raw Text. Existing text, paste, key and new Compose IPC structures redact bodies in Debug and clear owned character/text fields when dropped.

The desktop editor defaults to the current terminal. A current-input review freezes exactly one tab/session and does not add targets or broadcast to an existing armed group. Explicit Armed selection reuses ADR-0019's frozen group. Both paths retain the owned action for a single-use, 30-second review and recheck identity/security before dispatch and before IPC writing. Switching source, closing, losing focus or disarming clears the draft/review and revokes pending writes. The editor is bound to its originating window and source tab. Text editor IME output takes priority over terminal candidate-window positioning.

All Compose sends show fixed targets, visible LF/CR/TAB/control and bidirectional-format characters, and the separate Enter setting. Multiline, controls, optional Enter and recognizable destructive commands require an additional acknowledgment. This classifier does not determine arbitrary shell safety. Draft bodies are held in Zeroizing storage and are not autosaved; this does not promise erasure of every copy in allocator, UI layout or terminal buffers. Queue receipts and subsequent daemon failures remain separate and never imply exit status or success. Reviewed current input uses the same metadata audit policy as synchronized input, with no group ID and no body/title/hash.

## Saved command templates

The first structured actions are SendText and OpenCompose. SendText still enters mandatory review; OpenCompose opens an editable draft. Destination hints are Current or Armed, never persisted runtime session IDs. Commands carry stable IDs, folders, tags and supported-system metadata. RunTask/RunScript remain planned integrations with the task engine and QuickJS; this slice does not replace their scope.

The application validates versioned JSON, unique IDs, references and acyclic folder parents. Limits are 256 commands, 64 folders, 32 tags per command, 256-byte metadata labels, 256 KiB bodies and a 1 MiB serialized catalog. Unknown action/field/version, dangling references and deletion of referenced folders are rejected. Debug redacts saved template bodies.

The daemon owns SQLite catalog storage with a dedicated CAS revision. Schema 10 adds quick_command_state. Every supported old-schema upgrade creates a backup and performs its complete upgrade in one transaction. Failure does not commit an intermediate schema version. Existing profile/workspace/clipboard revisions and data remain independent.

QUICK_COMMAND_CONTROL at bit 27 gates Load/Save envelope payloads 43/44. Save validates the full catalog before the transaction and checks the expected revision. Desktop catalog operations run on a bounded background worker with a five-second deadline. Saves verify the exact returned catalog and next revision. Conflict, timeout, missing/malformed/mismatched reply or connection loss pauses writes until explicit reload; mutations are never automatically replayed or overwritten. Reloading updates the catalog without executing a command.

Saving requires explicit acknowledgment that template bodies are ordinary persisted configuration. No password/key material is intended for this catalog. The temporary editor and the saved template library have distinct persistence rules.

## Verification boundary

Automated tests cover production encoding with bracketed and legacy modes, current-only and armed routing, one-use review/security/identity/expiry/audit behavior, IPC privacy and capability checks, validated catalog edits, transactional schema migration/CAS/backup/reopen, headless desktop editing and an authenticated lost-reply/conflict worker fixture. Owned native local PTYs verify current-only Compose while Armed and selected-target Compose end to end. Native GUI/IME/DPI interaction, actual different-mode external SSH targets and the full platform acceptance matrix remain open.
