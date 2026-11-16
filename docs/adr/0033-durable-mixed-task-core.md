# ADR-0033: Durable per-host mixed task core

- Status: Accepted
- Date: 2026-10-09

## Scope

Add a distinct `cshell.mixed-task` v1 snapshot and ordered per-host ledger for
command, upload and download steps. This iteration implements the application
runner, SQLite storage and daemon internal service. It does not yet expose a
mixed-task IPC capability, CLI command or GUI entry point. Existing standalone
command/upload/download formats and their control surfaces keep their meanings.

A definition contains up to 32 typed actions and a parent concurrency/queue/total
policy. Each embedded command definition has exactly one command and concurrency
one, with no nested batch, canary, approval or failure-threshold policy. Embedded
file definitions also use concurrency one. The parent controls at most 50 host
workers; each host executes its own steps in order and stops at a non-success.
Different hosts can progress independently. Parent canary/batches/check/template
steps, explicit failed/Unknown repair and retry policies remain later work.

Creation freezes one Host/Profile closure, original shell definitions/parameters,
file paths/content policies, operator and creation time. A unique child UUID for
each host/step is allocated once inside the immutable snapshot. Child snapshots
are derived from the original single-host subset and fixed UUID/time, never the
current catalog or a newly generated ID after restart. Definitions are limited to
1 MiB, snapshots to 12 MiB, rows to 128 KiB and targets to 1,000. Repeated literal
upload destinations and download directory/basename pairs are rejected; filesystem
aliases are still checked by the actual no-replace publication ports.

## Typed receipts and storage ownership

MixedTargetRun records its revision, state, current cursor and all step states.
Each step preserves its native receipt: command TaskRunMeta/TaskTargetRun, an
UploadTarget or a DownloadTarget. Files retain progress, durable ownership, verified
content, publication intent, commit, adoption and cleanup fields. File transfers
do not receive synthetic SSH exit codes. Only a completed successful native receipt
can advance the cursor; later steps remain unstarted after failure, timeout,
cancellation or Unknown. Committed files with unfinished cleanup retain their
receipt and stop the sequence rather than being treated as uncommitted data.

Scoped leaf repositories bind parent fingerprint, Host ID, current step and child
UUID. Existing leaf schedulers commit into the parent row through opaque patches.
They cannot create/discover unrelated runs or update another slot. Child ledgers
are embedded in mixed_targets rather than independently mutable standalone tables,
so standalone task start/recovery APIs cannot bypass mixed ordering. Snapshot bodies,
credentials and paths do not enter metadata events or Debug output.

Schema 15 adds immutable mixed_runs, CAS mixed_targets and same-transaction
mixed_events. Snapshot SHA-256, display columns, identities, ordering, cursor,
receipt types, time bounds and native leaf transitions are checked on load/commit.
A patch is opaque and carries the expected snapshot fingerprint and whole before
row; the update and event are atomic. Older schemas create the new tables in their
original upgrade transaction; v14 backs up and adds them in one transaction. No live
database is migrated during this coding iteration. Old migration fixtures still
require the consolidated update/verification pass.

## Execution, cancellation and restart

Persist parent step intent before invoking its child runner. CommandTaskScheduler,
BatchUploadRunner and BatchDownloadRunner retain their existing durable intent,
streaming, host-key, Vault binding, filesystem and publication behavior. The parent
holds a run owner but does not hold a connection lease while calling a leaf, so
children acquire the shared global/endpoint/jump budget without recursive leasing.
Large parent snapshots are shared by Arc across host workers.

The queue deadline includes waiting behind parent concurrency. Each child retains
its own queue/connection/step or transfer deadline. The parent total deadline
signals all active children and stops later dispatch. Command cancellation forwards
the actual reason to its existing control token. Workers wait for leaf completion
and cleanup instead of discarding an in-flight native mutation. Storage errors stop
further dispatch; leaf and parent CAS replies are not automatically replayed.

Orphan recovery runs only under the parent owner without a live runner. It recovers
native command/file intents conservatively. A durable successful child can close its
parent step; known unstarted work can return to Pending for a separate confirmed
start. Unknown stays Unknown and prevents continuation. No native operation occurs
during recovery and no newly generated child ID hides a prior uncertain operation.

MixedTaskService uses the production SSH/file executors and the existing shared
budget. Its internal create/get/start/cancel methods bind start to the exact displayed
Pending Host ID/revision map, cap live parents at 64 and keep execution independent
of a caller connection. Public IPC/CLI/GUI integration and mixed output/result paging
are the next coding stage, including exposure of embedded file repair operations.

## Status

Code only, per the user's instruction; source formatting only. Compilation, tests,
schema fixtures, cancellation/storage-loss/restart races, large-plan costs, output
ownership, real SSH/SFTP and GUI/native validation remain for the consolidated pass.
The current daemon, live DB and original package-lock.json remain untouched. No gate
or whole P2 milestone is closed by this implementation.
