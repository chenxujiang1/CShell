# ADR-0030: Desktop batch upload control and recovery jobs

- Status: Accepted
- Date: 2026-10-09

## Decision

Add typed upload status/list/inspection documents shared by the daemon and desktop.
The desktop has an independent bounded IPC worker, a background source-manifest
hash operation with cancellation, and a virtualized per-target result view. Users
save a plan, inspect frozen destinations and content digest, then explicitly start.
Recovery binds run, Host ID, target revision and operation. Changed inputs or target
selection clear confirmations. Lost mutation replies disable further mutations
until an explicit reload; no writes are automatically replayed.

UploadList (operation 19) pages at most 64 persisted run summaries and shares
TASK_UPLOADS at IPC minor 29. Schema 13 is still an unverified local implementation; add immutable
name/creation-time summary columns to its not-yet-deployed upload_runs table.
No live schema 13 database is changed by this coding iteration.

Large Probe/Cleanup/Adopt requests launch bounded daemon-owned recovery jobs and
acknowledge launch only. UploadGet returns the latest job's bound host/revision,
operation, status and optional readonly probe result. At most 64 latest-job entries
are retained; active jobs are never pruned. Client disconnection does not cancel
them. Explicit cancellation can stop a job; the runner attempts channel cleanup
before releasing its owner/budget. Probe checks the bound target revision under
its run owner. Durable upload intent/receipt remains
the authority when the result is uncertain. Recovery-job UI state is ephemeral and
is not treated as a durable publication receipt after daemon restart.

Source hashing and local file access stay off the UI thread. The shared local
runtime helper rejects symlink/reparse/nonregular files, checks metadata and size
around a 32 KiB streaming SHA-256 calculation, and does not send file bytes over IPC.
The daemon still verifies transferred content before publication; manifest hashing
does not physically freeze a source against later edits.

Desktop controls expose grouped phases, bytes, ownership, verification, confirmed
publication, explicit adoption and temporary cleanup separately. They do not show
file transfers as SSH exit codes. Matching destination data is an inspection result;
adoption requires a separate explicit confirmation. Cancel bypasses the ordinary
worker queue. Queued and active upload control share the existing daemon policy.

## Status

Code only per the user's instruction. Compilation, tests, GUI/native workflows,
migration fixtures, large-file deadlines and recovery races are deferred to the
consolidated verification pass. Downloads and mixed task steps remain later work.
