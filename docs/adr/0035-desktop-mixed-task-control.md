# ADR-0035: Desktop mixed task editing and control

- Status: Accepted
- Date: 2026-10-10

## Editing and frozen review

Add MixedConnection and MixedPanel to the existing workbench, with toolbar and
current-host-selection entry points. Each window keeps its draft/selection/filter/
confirmation state while sharing a bounded authenticated control connection.
Switching modes clears confirmations and resets cross-panel log decorations.
Mixed editing uses the existing task-panel raw-input and synchronization isolation.
Toolbar items wrap so additional task entry points remain visible at smaller widths.

The builder supports ordered command/upload/download actions, up to 32 steps,
move/remove/add, a parent concurrency/queue/total policy, per-step deadlines and
Host selector/group/panel selection. Command forms create one static-script leaf
with explicit POSIX/PowerShell/Cmd and success codes, no nested gates. File leaves
use concurrency one. Richer Invocation/variables/environment definitions created
through CLI can be loaded and reviewed; this first GUI builder does not import or
edit those advanced definitions.

Upload fingerprints use the existing background 32 KiB helper with cancellation.
Draft keys are unique across windows and remain stable when a step is reordered.
Changed source/path/name/policy invalidates a returned manifest for saving; removed
steps cannot consume another step's result. At most 32 prepared manifests are kept.
No local source access, file publication or SSH runs on the UI thread. Background
preparation failures do not falsely mark an unsubmitted plan as an uncertain write.

Host preview is cached by selection and catalog/profile revisions. Saving checks
returned definition, selection, both revisions, every frozen target display field
and run identity. The user then reviews the saved command source/shell/success
codes/variables/environment, upload source/destination/digest, download directory/
name and all frozen host endpoints. Saving does not start a task. Startup binds the
saved snapshot and the whole current Pending revision list, independent of visible
page filters; it never starts only a silently selected subset.

## Status, cancellation and uncertain replies

Use existing IPC minor 31 / MIXED_TASK_CONTROL without a new schema or dependency.
Status pages remain at most 64 targets with run/Host cursors, complete Pending
confirmation, per-state totals, recorded step cursor and native typed receipts.
The UI explicitly labels current-page filtering and independent page observations.
Unknown, confirmed publication and unfinished cleanup remain distinct. There is no
GUI retry or file-repair button until its parent-bound protocol is implemented.

MixedStatusPage.check_snapshot binds each report to the loaded immutable plan,
including fingerprint, definition summary, policy, revisions, target display
metadata, child IDs and full native row validity. The worker performs this check
before publishing state to the UI. Paging and output requests remain bound to the
currently selected shared run; an older window's poll cannot replace a newer run
selection. Explicit inspection selects a run. Active work and a lost launch/cancel
reply trigger bounded read-only status polling, never replay a mutation.

One ordinary queue slot and one independent cancellation slot keep control bounded.
Cancellation is bound to the displayed run and blocks concurrent writes until the
reply completes. Unknown write/cancel replies advance an authority epoch; only an
explicit reload queued in that same epoch and after cancellation can restore writes.
Create replies, automatic reads and older reloads cannot restore authority. Closing
the pane/window does not cancel daemon execution; cancel is a separate user action.

## Output display

Only dispatched command steps offer stdout/stderr reading. The request binds parent,
Host ID, mixed index and expected child UUID derived from the reviewed plan; the
worker verifies the returned context and optional exact target revision. Live reads
use the latest revision, inactive reads use the currently loaded target revision.
Pages are at most 64 KiB, explicitly refreshed/paged, and never advertised as complete
output or an atomic metadata/byte snapshot.

Reuse the existing background byte escaper and wgpu LogSurface instead of creating
a terminal or large text widget. Controls, non-ASCII bytes and backslashes remain
unambiguous. A bounded reflow worker holds one pending request and completed layout.
Changing run/target/request clears old output, and log page revisions prevent stale
layout results from replacing a newer page. Existing command output code and mixed
output share the formatter/reflow implementation.

## Status

Source implementation and formatting only, as requested. No compilation, tests,
GUI/native windows, migration, daemon replacement or real SSH/SFTP/hash actions are
performed in this iteration. Existing daemon/live DB/package-lock.json remain
untouched. Mixed parent-bound file repair, richer result/export/archive, batch/
canary/check/template steps and the full Phase 2–4/native verification scope remain
in progress. Old verification evidence does not cover the current WIP.
