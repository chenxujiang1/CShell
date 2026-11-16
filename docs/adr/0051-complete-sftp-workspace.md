# ADR-0051: Complete SFTP workspace coding

- Status: Accepted (implementation written; verification deferred)
- Date: 2026-10-12

## Scope

Complete the remaining P3-010 coding after ADR-0048/0049/0050: managed conflict and
link policies, concurrency/rate limits, reviewed migration to a new authenticated
session, file management, drag/drop, directory trees and remote editing. Desktop,
CLI and daemon use the same application documents and existing session authority.
The user requires compilation, tests, audits, native UI and real SFTP verification
to run together later. Coding completion does not close an acceptance gate.

## Managed queue and publication

Optional/default-omitted fields extend the existing version-1 queue without changing
legacy canonical JSON/checksums. Drafts can bind source/destination size/SHA-256,
destination permissions, expected permissions, Follow, bytes-per-second rate and
the existing FilePublicationPolicy. Advanced drafts require verified execution.
Add/edit/configure operate by exact revision; managed add and bulk add leave the
queue paused. Bulk accepts 1–128 verified ordinary-file drafts in one metadata save.
An editor-bound Pending draft cannot be edited as a normal queue row.

Paused queue configuration permits 1–4 native workers. More than one requires
verified Pending/Running rows and distinct literal destinations. A JoinSet dispatches
under the queue lock, saves Running/native ID before spawn, protects every current
native job until its receipt is saved and merges each receipt with current metadata.
Known pre-spawn refusal can restore Pending; uncertain native effects cannot.
Failure pauses future dispatch and drains already running workers. Pause does not
cancel active work. StartOne and repair operate on one row with other Pending paused.
Global limits remain eight native jobs and eight queue owners; exclusive file/edit
jobs reserve the same budget and cannot overlap native work on their session.

Verified transfers keep the original full-source/full-temporary/prefix checks,
32 KiB write/fsync checkpoints and durable Create/Write/Publish/Cleanup boundaries.
They reuse P2 publication ports for no-replace, skip-identical and backup-replace.
The immutable previous digest, skip receipt and verified independent backup receipt
are saved with the journal. Replacement requires explicit exclusive-write acceptance;
unsupported fsync/hardlink/posix-rename never falls back. Resume rechecks the saved
target plan; unknown publication is probed/adopted, never resent. Adoption of an
existing replacement also checks its backup. Backup names derive from the item UUID
beside its temporary; the UI displays whether they are planned or verified.

Upload mode changes use a separate Permissions intent, confirmed before publication.
Editor uploads preserve ordinary rwx only and compare the old target mode; special
privilege bits, owners and ACLs are not copied. Rate pacing uses confirmed offsets
with at most an initial chunk burst and retains the 24-hour worker deadline.

Managed transfers hold namespace read locks on local and peer scopes and a normalized
destination mutex; file mutations hold a namespace write lock. Keys and caches are
bounded/redacted. Local parent canonicalization and conservative remote case folding
serialize recognizable aliases. These locks do not claim control over external
writers, mount aliases or the independent P2 task executors.

## Reviewed migration

Managed queues capture host/port/account/verified host-key/captured-route identity.
Migration requires explicit confirmation, exact source and target revisions, the
same authenticated peer, paused queues, no queue owner/running native work and an
empty target. Both queue locks are ordered by session ID.

Persist the source seal (target and payload digest) before writing target origin and
copied items. The copied queue preserves item UUIDs, temporary/backup names, receipts
and journals and stays paused. Sealed source mutations are refused. Target starts
validate its origin against the source seal; chains carry separate inbound and
outbound bindings. An identical explicit retry may finish a sealed migration after
target storage failure; it does not replay native work. No auto reconnect, credential
resolution or native dispatch follows migration.

Legacy queues without captured peer can bind only while the original authenticated
session is still available. A dead session with no saved identity is not sufficient
authority for migration. Editor rebind is separate: its queue must reach the new
session through a valid migration chain, or its unchanged remote base must pass a
read-only digest check on the same peer.

## File management and browsing

FileInspect persists an Inspecting job and starts a budgeted read-only worker.
Ready records show source/target type, metadata, file digest and link target.
Confirmed FileApply binds ID/revision/session/peer, takes the namespace lock,
rechecks the preview and saves intent before native mutation. Successful ACK is
stored independently; cancellation, lost ACK or orphan work becomes Unknown and
is never automatically applied again.

Actions are inspect, mkdir, rename, delete, chmod and symlink on local or remote
paths. Delete removes a file/link or empty directory, never recursively. Local
rename uses Linux renameat2(RENAME_NOREPLACE), macOS renamex_np(RENAME_EXCL), or
Windows MoveFileExW without replace flags; unsupported platforms refuse. Remote
rename uses ordinary SFTP v3 rename after destination inspection. This is not a
server-side CAS. Windows local chmod is rejected rather than emulating Unix modes.

Managed browsing uses the original cursor reservation/deadline/lifecycle. NoFollow
is default; Follow resolves the requested directory and then uses canonical
NoFollow checks. Follow source reads resolve before pinning; destination parents
remain restricted. Links are displayed and inspectable without implicit traversal.
Directory trees cache visited directories and current page children, with bounded
nodes and no recursive UI I/O. Lists remain forward, native-order enumeration and
do not promise an immutable remote snapshot. Drag/drop transfers ordinary files;
directory copy/sync/recursive delete are outside this implementation.

## Remote editing and retention

Open creates durable Preparing metadata and downloads a fully verified private
working copy, at most 8 MiB. The controlled working name contains the document UUID
and a bounded alphanumeric extension. UTF-8 text at most 256 KiB can be edited
inline; other supported files use an external editor. Executable paths are absolute,
arguments are literal (exact {file} substituted or path appended), with no shell.
At most 16 known editor processes run, one per document; child completion is reaped.

Read-only polling detects working-file changes and does not upload. Save binds the
document and queue versions, checks the displayed current digest, writes an inline
draft or reads the bounded pinned working file, and freezes a private upload copy.
Before appending its deterministic queue item it persists the editor's queued intent.
The draft binds frozen source and original remote base SHA/mode and requires explicit
backup-replace acceptance. Metadata retries may complete this append without adding
a duplicate; Unknown native work is never resent. StartOne returns other Pending
to their paused state. Successful durable receipts update the editor base; failures
retain the working draft and recovery information. Dirty UI text survives reads and
session switches until confirmed saved content or explicit reload.

Workspace records use private checksum JSON, bounded regular no-reparse files,
file locking/revision CAS, private temporary sync/atomic replace and Unix parent
sync on blocking workers. Maximum 1,024 record directories, 64 history rows per
page and 16 frozen upload copies per editor. Closing preserves drafts and does not
kill the editor process. Forget requires terminal state, exact revision and no
known active job/editor. Explicit private discard deletes only known regular files,
never recursively or while an upload remains referenced; keeping files is supported.

## Protocol and clients

Minor 46 / SFTP_WORKSPACE bit 50 adds operations 22–42:

| Operations | Purpose |
|---|---|
| 22–23 | QueueEdit, QueueMigrate |
| 24–27 | FileInspect, FileApply, FileJobGet, FileJobCancel |
| 28–32 | EditOpen, EditGet, EditLaunch, EditSave, EditClose |
| 33–37 | QueueAddManaged, BrowseManaged, QueueStartOne, QueueStartManaged, QueueConfigure |
| 38–42 | WorkspaceList, WorkspaceForget, PeerGet, EditRebind, QueueAddMany |

Existing SFTP_CONTROL/browser/verified capabilities are required, and BrowseManaged
also requires directory cursors. Workspace replies are bounded tagged application
documents; clients validate operation/scope/ID/revision and editor text digest.
Desktop mutation authority binds session/revision/epoch; unknown writes revoke
authority until explicit same-scope reload. Background polling neither restores
write permission nor clears a pending mutation. The CLI sends mutations once with
--confirm and exact versions; workflow examples are in Phase 3 documentation.

No SQLite migration or dependency version upgrade is needed. Windows local rename
adds a feature to the existing windows-sys dependency. Legacy direct transfer ports
retain their behavior and do not silently become managed operations.

## Verification still required

Only source review and cargo fmt have run. Compile/type/MSRV/lint/test, old wire and
canonical bytes, all bounds and resource reservations, CAS/storage faults, every
intent/ACK/cancel/orphan boundary, migration partial writes, parallel destination
aliases, source/target/backup changes, GUI scope/dirty draft behavior, CLI workflows,
platform rename/links/reparse and real server extensions remain unverified.
Name-based probes cannot prevent all external filesystem races; an unconfirmed
effect stays Unknown. Tokio deadlines cannot stop blocked local kernel I/O.
