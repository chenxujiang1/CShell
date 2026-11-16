# ADR-0029: Durable multi-host upload core

- Status: Accepted
- Date: 2026-10-08

## Scope

P2-070 first implements a distinct upload run kind with frozen Host/Profile closure,
one declared local source size/SHA-256, a fixed absolute remote destination, bounded
concurrency and per-target durable state. It does not encode uploads as shell commands
or invent SSH exit codes. Source bytes are streamed; changed content fails before
publication. Existing command TaskRuns and Phase 1 single-file transfers keep their
formats. Upload runs share the command scheduler's connection budget and ownership
limit. Download, backup/replace, generic mixed-step tasks and desktop integration
remain follow-up work.

## Publication and recovery

The remote temporary filename derives from run and host UUIDs in the destination
directory. Persist Creating before exclusive creation, ownership before writes,
Verifying before readback and PublishIntent before publication. Stream at 32 KiB;
checkpoint acknowledged progress at 1 MiB boundaries. Read back the full temporary
file and compare size/SHA-256; require fsync@openssh.com and hardlink@openssh.com.
Publish by a no-replace hard link followed by removing the temporary name. Missing
extensions are refused before mutation; no overwrite-prone rename fallback is used.
File-data sync is distinct from remote directory power-loss durability.

Lost replies, canceled/timed-out mutations and daemon restart preserve Unknown.
Failed writes retain their temporary artifact for explicit inspection/cleanup.
Only acknowledged, durably recorded temporary ownership permits cleanup. A creation
with lost acknowledgement cannot be assumed owned. PublishIntent uncertainty is
never replayed. Matching destination content is an inspection result, not automatic
proof of this run's publication. Explicit adoption may resolve a saved publish intent
after full digest verification; cleanup never removes the final destination.

Each target has an independent CAS revision; transitions and metadata-only events
commit in one SQLite transaction. Schema 13 adds immutable upload snapshots, target
rows and transition events. Existing databases are backed up before migration;
the new tables are included in the same transaction as earlier schema migrations.
Storage uncertainty stops further network mutations without retrying commits.

## Daemon and CLI

IPC minor 28 / TASK_UPLOADS bit 33 adds UploadCreate/Get/Start/Cancel/Resume/Probe/
Cleanup/Adopt within TaskRequest. Create freezes both catalog revisions and saves
the plan without connecting. Start and per-target recovery require explicit
confirmation; recovery binds the row revision. Start/resume are daemon-owned jobs
independent of the requesting client. Cancellation of an active run bypasses the
ordinary operation mutex. Get recovers orphaned active phases without replay.
Long readonly probes and cleanup/adoption can exceed the IPC deadline; an unknown
reply requires inspection, never a repeated mutation. The CLI can generate a source
manifest using streaming local hashing, then create/query/control the saved run.

No automatic overwrite, backup/replace fallback, auto-adoption or unknown-publication
retry is offered. This slice has a CLI/backend entry; the desktop and mixed-step
workflow are still subsequent implementation.

Protocol reference: [OpenSSH SFTP extensions](https://github.com/openssh/openssh-portable/blob/master/PROTOCOL).

## Status

This is code only per the user's instruction. Compilation, regression tests, native
servers, recovery faults, capabilities, races and filesystem durability are deferred
to consolidated verification. This core does not close P2-070 or P2 acceptance.
