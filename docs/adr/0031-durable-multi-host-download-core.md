# ADR-0031: Durable multi-host download core

- Status: Accepted
- Date: 2026-10-09

## Scope and execution

P2-070 adds a distinct download run alongside command and upload runs. A saved
plan freezes Host/Profile closure, absolute remote source, daemon-side local
directory, a simple ASCII local filename, maximum bytes, concurrency and queue,
connection and transfer deadlines. Creating a plan performs no source/destination
file or SSH I/O.
The caller must explicitly start the saved plan. Downloads use the existing
frozen credential binding and strict actual-target/jump host-key verification.

The local directory must exist. Each final name is `<host-uuid>-<local-name>`;
different targets do not overwrite one another. Temporary names contain both
run and host UUIDs. Names cannot introduce directory traversal, separators or
Windows alternate streams. Symlink/reparse source entries and local directory
components are rejected. There is no recursive directory creation or deletion.
Local paths are meaningful on the daemon computer. Bytes do not travel through
control IPC; the daemon writes the file as part of the confirmed task.

Each target first fingerprints a held readonly SFTP handle, then durably pins
that target's size/SHA-256 before creating its local temporary file. Different
hosts may have different content. The file is read in at most 32 KiB chunks,
with a 1 TiB maximum per file, at most 1,000 targets and at most 50 concurrent
connections. Runners share command/upload global, endpoint and jump budgets.
Queue time includes waiting behind the run's own concurrency. Snapshots are
shared by worker references rather than copied once per concurrent target.

Before publication, the received stream digest must match the pinned content;
the local file is synchronized and fully reread, and the held remote handle is
fully fingerprinted again. Local metadata captured during verification is checked
again before publication. Full rereads detect content changes but do not provide
an atomic snapshot of a file concurrently modified by the server or another
process. Handle/path races and same-UID metadata restoration remain part of the
filesystem/server verification matrix; this iteration does not claim isolation
from another writer with the same operating-system authority.

## Durable receipt and recovery

Schema 14 adds immutable download snapshots with SHA-256, compact list columns,
per-target CAS state and same-transaction metadata events. Creation, ownership,
content checkpoints, publication intent, confirmed commit and cleanup are separate
durable transitions. A progress checkpoint synchronizes bytes before recording
the acknowledged offset; progress is not a publication receipt. Unknown remains
distinct from failure or successful completion; transfers never invent SSH codes.

Publication uses a same-directory hard link that fails if the final name exists;
there is no overwrite or rename fallback. Unix directory sync precedes successful
publication/cleanup acknowledgement. Windows and nonlocal filesystem power-loss
durability still need native verification. A lost local mutation or repository
reply retains uncertain intent; a confirmed publication with unfinished cleanup
remains Committed/CleanupRequired. The final file is never removed by cleanup.

Explicit resume binds host and target revision. It requires durable temporary
ownership, unverified content, and no uncertain publication. Reinspect the source
against the pinned digest; compare the entire existing local prefix byte by byte
before appending and repeat all publication checks. No source substitution or
automatic replay is allowed. Unknown creation without acknowledged ownership
cannot be resumed or cleaned automatically; its orphan must be inspected outside
the managed recovery flow rather than guessed to belong to this run.

Probe examines local temporary/final contents without opening SSH. Cleanup and
adoption require separate confirmation and the exact target revision. A matching
final digest is not proof this run published the file; explicit adoption records
that decision and only then allows temporary cleanup. All recovery operations are
daemon background jobs with bounded latest-job state and explicit cancellation.
Job hints are ephemeral; durable target receipts remain authoritative after restart.
Closing a client does not cancel execution. An explicit cancel reaches active
workers directly or marks unstarted targets Cancelled. Neither action deletes files.

## Control surface and status

IPC minor 30 / TASK_DOWNLOADS bit 34 adds Task operations 20–28:
DownloadCreate/Get/List/Start/Cancel/Resume/Probe/Cleanup/Adopt. Canonical requests
reject unrelated fields; Start/Resume/Cleanup/Adopt require confirmation. Lists
are limited to 64 summaries. Status contains frozen target display metadata,
the definition, bound revisions and durable rows, excluding authentication
templates, secrets and file body. CLI adds download-definition plus matching
create/get/list/start/cancel/probe/resume/cleanup/adopt commands. Clients send
mutations once and verify typed response identity and list cursors.

Implementation only, per the user's instruction. No compilation, tests, live
migration, GUI/native or real transfer verification is performed in this iteration.
Existing schema 13 code is unverified and undeployed; migrations for older schema
versions now create download tables in the same original transaction, while v13
adds them after backup in one transaction. Fixtures and the complete migration,
cancel, lost acknowledgement, disk-full and server matrix await consolidated
verification. Desktop download controls, generic mixed task steps, download
backup/replace policies and broader Phase 2–4 scope remain follow-up work.
