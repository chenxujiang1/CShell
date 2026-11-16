# ADR-0061: Reviewed workspace recovery and pane separation

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-13

## Scope

Complete the listed P3-040 workspace separation/recovery coding alongside ADR-0058
batch edits and ADR-0059/0060 context inheritance. The existing cshell.workspace v1
document and SQLite WorkspaceRepository revision CAS remain the storage contract.
There are no new wire fields, capabilities, database schema or dependency versions.
WORKSPACE_CONTROL already provides authenticated Load/Save operations.

Recovery restores layouts and saved Profile references. Files contain no SessionId,
credential, program, command, runtime terminal context or queued input. No shell,
SSH connection or previous command is automatically started. The user explicitly
opens placeholder tabs afterward through the normal saved Profile launch gates.

The application layer adds a bounded WorkspaceRestorePlan containing the full local
before document, target document, expected storage revision and deterministic sets
of tabs eligible for retention, views to detach and new placeholders. Retention
requires identical TabId and ProfileId; the desktop additionally requires an existing
runtime binding. Titles or destination windows can change without changing process
identity. Removed or rebound tabs retire their UI connections without sending view
close/terminate requests, even for TerminateOnViewClose Profiles. Daemon processes
continue independently. A restarted application has no old runtime bindings to retain.

Layout limits remain 256 KiB / 256 tabs / eight windows; desktop recovery also
requires at most eight leaf panes per window and nonnil IDs. Unknown fields, invalid
topology, cycles, orphans, duplicate IDs and missing layout references reject. Plan
serialization is bounded to twice the layout limit plus 32 KiB; canonical SHA-256
and recomputation validate the typed plan. Missing Profile references are displayed
as unavailable placeholders, rather than substituted with another Profile.

## Autosave read barrier and reviewed commit

All desktop windows share one WorkspaceConnection. Only the primary submits
autosaves; secondary mutations are absorbed by the window hub. Autosaves remain
coalesced, sequential and revision-bound. Replies now prove exact submitted document
and revision+1 before authority advances. A lost, invalid or conflicting save pauses
autosave rather than replaying it.

Opening recovery immediately pauses submissions in every window. A serialized
control read waits for any earlier in-flight save, discards only unsent autosaves,
then loads fresh storage authority. Reads do not replace the local layout or resume
saving. The user chooses the current local draft, latest stored layout, imported
file or edited JSON, then reviews the full target and binding effects. Keeping the
current draft is an explicit overwrite of the freshly read revision, not an automatic
rebase. Closing the panel leaves autosave paused.

Confirmation submits the frozen target through the existing SQLite transaction/CAS.
The worker checks the complete receipt and retains it for window-hub adoption.
The hub rechecks that the local before document is unchanged and no window is
creating a session or closing a view. Only then does it adopt the committed layout
and resume autosave. If local state changed after submission, the committed receipt
does not overwrite it automatically; autosave stays paused and the user reads and
previews again. No unknown write is retransmitted. The review remains client-side;
the daemon enforces its existing typed layout validation and SQL revision CAS,
without a persistent preview token or cross-client editing lock.

During recovery, all windows gate workspace/session creation, selection and layout
mutations. Pending launches/closes block entering recovery. Input/IME/search and
accessibility guards include the recovery panel; synchronized input is disarmed and
old window membership removed before adoption. Retained surfaces/connections are
redistributed by actual destination window, stale views are retired, obsolete child
windows close and new windows are reconciled. Temporary themes remain existing
memory bindings, never file data. Native window failures retain the saved layout;
the explicit retry action clears the unavailable-window suppression and retries.

## Pane separation

Detach current pane moves the entire focused leaf tab group into a new native
window, retaining group/tab/session IDs and cached view state. For a split pane,
its parent is collapsed onto the sibling subtree without dropping that subtree's
split structure. If it was the only pane, the old window gets an empty group.
Focus stays valid in both windows. The operation validates a cloned complete
document before publishing; no partial layout mutation is retained on failure.

The window hub transfers every tab's cached view and connection, then restores the
group's original active tab. It does not clone or restart a process. A failed target
window leaves the saved layout and daemon sessions available for explicit retry.

## Files and CLI

Desktop import/export runs on a separate bounded worker; late file results are bound
to request epochs. Inputs are regular, bounded JSON and accept a UTF-8 BOM. Export
uses a synced temporary file and refuses an existing destination. File data has
the existing v1 layout format, so older readers can use supported layouts. JSON
paths and metadata are exposed only through the explicit file UI, not terminal input.
Native filesystem operations are bounded in data size, not guaranteed wall-clock
latency; network filesystems and fsync behavior require platform verification.

CLI workspace get/export/preview/restore --confirm uses authenticated discovery,
40-second requests, bounded regular inputs and no-clobber output. A preview artifact
contains the exact stored before document and revision; restore rereads both before
the CAS save and checks the complete receipt. CLI changes the persisted layout,
not an active GUI's runtime map. A concurrent GUI detects revision conflict and
recovers explicitly; it does not silently adopt external edits. No request starts
or terminates a process.

This is layout recovery from valid saved/imported documents, not damaged SQLite
repair, secret export, full process resurrection, terminal-output recovery or replay
of commands. Window geometry and native display placement remain toolkit-owned
and are not added to the stable v1 format.

## Deferred verification

Per the user's instruction, only code edits and cargo fmt were performed. Compilation,
tests, audit/MSRV, native GUI/PTY and platform fault verification remain in the
unified batch. Include old layout/wire compatibility, plan tampering/nil/limits,
deep/single/sibling split detachment and focus, retained/rebound/missing Profile IDs,
autosave coalescing/read barriers/late receipts/CAS/unknown writes/shutdown, two-window
controls and pending creation, sync/IME/input isolation, obsolete/failed/retried
windows, retained process identity and no implicit CloseView/launch, file worker
epochs/BOM/regular-file/no-clobber/fsync, and CLI/GUI concurrent writers.

Usage: [WORKSPACE_RECOVERY](../phase3/WORKSPACE_RECOVERY.md).
