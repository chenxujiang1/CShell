# ADR-0058: Profile batch metadata edits and inheritance inspection

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-13

## Scope

Continue P3-040 with atomic batch editing of Profile metadata/terminal overrides
and read-only inspection of the existing global→folder ancestry→Profile terminal
inheritance. Workspace/window transfer and basic restore remain the P1 foundation.
Advanced workspace recovery/separation and tag/Host/temporary policy inheritance
remain further P3-040 coding. This ADR does not close the entire P3-040 task.

The user requested code first and unified verification later. Only source edits
and cargo fmt have run: no compilation, tests, audit/MSRV, native GUI or real
SSH/SFTP validation. No existing daemon/database was restarted or operated.

## Declarative batch model

cshell.profile-batch v1 contains the expected Profile catalog revision, an explicit
unique set of 1–128 nonnil Profile IDs and a patch. The request is at most 64 KiB.
The patch supports:

- Name prefix/suffix, each at most 64 UTF-8 bytes; resulting names use existing
  catalog validation, including sibling uniqueness and total name limits.
- Folder Keep/Set/Clear, where Clear moves to the root.
- Tags Keep/Replace/Append/Remove with explicit set semantics. Empty Replace
  clears tags; Append/Remove use exact case-sensitive values.
- Favorite Keep/true/false.
- Terminal type, theme reference and logging Keep/Set/Clear. Clear restores
  inheritance, distinct from retaining the current explicit override.

IDs, Profile kind, SSH targets/routes/authentication/key references, local
program/arguments/environment/close policy and credential data are not patch
fields. Unknown fields are rejected. No connection, vault read, launch, remote
command or credential rebinding takes place. Theme display follows the existing
catalog refresh; running processes/connections remain owned by their sessions.

The pure application planner clones the validated catalog, resolves every selected
ID, generates only changed UpsertProfile operations, validates the entire proposed
catalog and resolves each selected Profile's before/after terminal values and
sources. Missing selections/folders, name conflicts, invalid fields, inheritance
cycles and entirely unchanged batches reject the whole plan. A selected unchanged
row can appear alongside changed rows; it is not rewritten unnecessarily.

## Preview, confirmation and transaction

The preview contains sorted exact before/after Profile records, before/after
resolved fields, catalog revision and a SHA-256 of the canonical draft. It has a
fresh nonzero 32-byte nonce, five-minute monotonic expiry and a wall-clock expiry
for presentation. A daemon retains at most 16 pending previews. Plans/replies are
bounded to 2 MiB; an oversized preview is refused before a token is retained.

Apply requires explicit confirmation, the nonce, the exact revision and a
SHA-256 of the entire canonical reviewed plan. The extra review digest prevents
an edited CLI preview file or a substituted UI review from confirming the server's
different retained plan. A matched token is consumed before expiry/revision/digest
or effect checks and is never restored after a refused/failed/uncertain attempt.
The nonce is process local: restart cannot recreate it from an exported preview.

The daemon shares ProfileIpcService's catalog mutation mutex, reloads the validated
catalog, recomputes the plan and checks exact equality before committing. Existing
ProfileService/repository transaction and global revision CAS apply all changed
records atomically. SSH/local records and secrets are retained by the same snapshot.
The reply describes the exact committed selected records at revision+1; a later
writer does not retroactively change this receipt. CAS/stale revision rejects and
clients prepare a new preview rather than rebasing or replaying the original apply.

Planning runs on blocking workers, not the terminal/parser or UI path. Pending
preview locks are not held over awaits; the async catalog mutation lock remains
held through planning/commit. Limits bound selection/replies/retained plans, though
catalog loading and native database work still use existing storage behavior and
need performance/fault verification.

## Inheritance inspection

ResolvedField, SettingSource and ResolvedTerminalSettings gain typed serialization
without changes to persisted Profile fields. Source values are global, folder UUID
or Profile UUID; field values and sources remain separate. Inspect accepts an
explicit Profile ID and optional exact revision and returns its saved record and
resolved terminal type/theme/logging. It is read-only and does not change defaults,
run a process, reconstruct a shell or claim new context-policy support.

Desktop inspection validates the returned record/settings against its captured
catalog. Previews are independently recomputed from that immutable catalog before
they become actionable. CLI validates draft IDs, canonical patch effects, request
digest and revision; Apply compares the exact receipt to the reviewed plan. Both
clients validate request identity, feature negotiation, reply type and size.

## IPC and interfaces

Minor 52 adds PROFILE_BATCH bit 56 and envelope tags 57/58. Operations are Preview,
Apply, Inspect and Reload. Every unused field must have its canonical default.
Reload reports the current validated catalog revision without executing changes.
All operations require the authenticated local handshake, the feature and current
desktop access. Older daemons are refused; no legacy batch simulation is used.

The desktop toolbar opens the batch/inspection window. It provides a bounded
filtered list (first 256 results), explicit selection up to 128, typed patch controls,
per-row diffs and inheritance sources. Selection/filter are separate; changing
fields, selection, revision or preview nonce clears confirmation. Late replies are
bound to captured edit/inspect epochs and cannot authorize changed forms. Expired
previews cannot be applied. Success requests a fresh Profile catalog read.

An uncertain Apply clears the preview and disables writing. Only explicit Reload
restores batch permission; inspection or other reads do not automatically restore
it. Revision must also match the ordinary Profile catalog. The original apply is
never automatically retransmitted. Keyboard/IME/search/accessibility/sync-input
guards include this window. Secondary windows use their actual daemon config.

CLI supports reload, inspect, preview-to-new-file and apply --confirm. Input files
are regular, bounded and allow UTF-8 BOM. Preview publication uses a private synced
temporary file without replacing an existing path. Apply computes the review
digest from the file and checks expiry; the server remains authoritative. Preview
tokens are omitted from Debug diagnostics, not from the explicit review artifact.

No SQLite schema or dependency version changes are made. Instructions/examples are
in [PROFILE_BATCH](../phase3/PROFILE_BATCH.md).

## Deferred verification

The unified batch must cover compilation/serde/protocol on all targets; exact
Keep/Set/Clear and tag modes; nil/duplicate/missing IDs, count/byte limits, Unicode
names/tags, no-op rows, collisions/cycles and rollback; before/after source resolution;
unchanged SSH/local/credential bindings; canonical request/draft/review hashes;
nonce expiry/consumption/budget/restart; edited/substituted review files; concurrent
writers/CAS, database failure and lost replies; UI late epochs/new nonces/confirmation,
reload/secondary windows/input isolation; CLI publication and structured receipts.
