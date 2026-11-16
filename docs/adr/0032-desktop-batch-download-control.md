# ADR-0032: Desktop batch download control

- Status: Accepted
- Date: 2026-10-09

## Decision

Add DownloadConnection and DownloadPanel to the existing task workbench. Command,
upload and download panels have explicit modes; changing mode or returning to the
host panel clears operation confirmations. File panels do not render retained
command output. The existing task-panel input isolation and synchronization disarm
apply to download editing and review. Each window keeps its own draft, selection,
filters and confirmations, while the authenticated connection and saved status
are shared across windows.

The download builder accepts a name, absolute remote source, existing local
directory, a portable basename, maximum bytes, concurrency and three deadlines.
Host selector, group and current host-panel selection are supported. Before saving,
show matched names, addresses, ports, Host IDs and each exact local destination.
Preview is cached by selection and Host/Profile revisions; source/path/policy edits
clear confirmation without rereading files. The application derives display and
execution paths with the same helper. There is no filesystem or SSH work on the
UI thread and no automatic directory creation.

Save checks the returned definition, both revisions, exact target display metadata,
new run identity and initial target states against the submitted plan. It does not
start a transfer. The user reviews saved destinations and policies and separately
starts Pending targets. Lists page at most 64 summaries and allow loading by run ID.
Targets are virtualized with text/phase filtering, known per-target size, confirmed
bytes, source SHA-256 and separate ownership, verification, commit, adoption and
cleanup indicators. Long list labels and paths are truncated with full hover text;
selected-target details display the complete paths.

Resume/Cleanup/Adopt eligibility is provided by DownloadTarget and used by the UI,
connection, daemon service and runner. GUI recovery requires the selected run, Host
ID and nonzero exact row revision. Probe is read-only and revision-bound. Before
adoption, a completed probe for that host/revision must match pinned destination
content; the user separately accepts uncertain publication attribution. The daemon
still rereads the file and checks the persisted revision before recording adoption.
Matching content itself is never an automatic success receipt.

## Connection ownership and uncertain replies

The independent worker has one ordinary operation slot and a separate cancellation
slot. Ordinary work cannot delay a cancel request. Cancellation is bound to the
currently loaded run; duplicate cancellation and concurrent mutations are blocked
while a cancel reply is pending. Read requests can continue. Workers never retry a
mutation or job launch after a timeout or lost reply. Closing the panel or window
does not cancel daemon execution. IPC uses the existing minor 30 / TASK_DOWNLOADS
capability and contains control/status, never download data.

Unknown mutation/cancellation replies advance a local authority epoch and disable
further writes. Explicit reload captures the epoch when queued and only restores
authority if it still matches and cancellation has completed. A stale reload cannot
override an uncertainty observed after it was queued. Automatic polling and create
replies cannot restore authority. UI control changes and saved document changes
clear confirmations. Active transfers, recovery jobs and cancellation are observed
with bounded read-only polling; lost launch replies prompt a read, never a replay.

Upload and download connections share FileTaskTransport for authenticated discovery,
capability negotiation, a 10-second request deadline, reply identity/status and
file-control response-field checks. Upload cancellation/reload now uses the same
epoch rule, preventing an older query or creation reply from undoing a later
unknown cancellation. No protocol/schema extension or dependency upgrade is needed.

## Status

Code implementation only, following the user's instruction. Only source formatting
is performed. Compilation, tests, migration fixtures, GUI/native input/window
behavior, cancellation/reload races and real SFTP/filesystem verification remain
for the consolidated pass. The existing daemon, live database and package-lock.json
are untouched. P2-070 and overall gates remain open; mixed task steps, backup/replace,
archiving and the remaining Phase 2–4 scope remain in progress.
