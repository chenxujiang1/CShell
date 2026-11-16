# ADR-0048: SFTP dual-pane browser and durable session queue

- Status: Accepted
- Date: 2026-10-12

## Scope

Begin P3-010 with daemon-local/remote dual-pane browsing and a durable session-bound
transfer queue. This follows P2 functional coding completion; compilation, automated
tests, native GUI and actual transfers remain deferred by the user. It does not close
P3-010 or introduce automatic retry/resume, remote editing or recursive file management.

## Browser

The existing SFTP window now shows daemon-local and remote lists side by side,
with explicit listing, parent navigation, directory double-click and regular-file
selection. LocalList uses an absolute directory or the daemon user's home for an
empty initial path. It rejects symlink/reparse directory components, uses lstat
entry metadata, identifies links without following them, and rejects non-UTF-8 or
oversized names instead of aliasing paths through lossy conversion.

The initial legacy lists are bounded to 512 entries and explicitly report truncation.
[ADR-0049](0049-sftp-directory-cursors.md) subsequently adds full forward cursor
enumeration to the desktop and CLI. Remote file operations and configurable link
policy remain later work.
The UI identifies the local pane as the daemon computer and shows the loaded
directory independently of the editable input. Listing runs off the UI thread.

## Queue documents and storage

Application SftpQueueDraft/Item/Snapshot/Receipt use a new strong SftpQueueItemId.
A snapshot binds an immutable existing SSH SessionId, independent positive revision,
paused flag and up to 128 entries. Drafts specify upload/download and bounded
absolute local/remote file paths. The queue freezes paths, not file contents:
contents are opened when the item is dispatched.

Pending, Running, Succeeded, Unknown, CleanupRequired and Cancelled remain distinct.
Cancelled represents an unstarted queue entry only. Native transfer cancellation or
an unconfirmed failure becomes Unknown; it does not prove an unchanged destination.
Receipt fields retain actual bytes/optional total, confirmed destination commit and
reported cleanup location/path. A failed size check can retain bytes above its
declared total. Debug redacts paths and cleanup text.

Private sftp-queues/<session>.json stores canonical snapshot bytes with a SHA-256
wrapper. Load validates file type/reparse protection, bound size, format, session,
revision, unique IDs, state shape and checksum. Writes use per-session async locks
and a private cross-process file lock, compare the saved revision, sync private
temporary content, atomically replace the JSON, and sync its parent on Unix.
Disk work runs in blocking workers. This uses existing dependencies and no SQLite
schema migration. Paths are ordinary private configuration, acknowledged before
queue insertion in the UI.

## Ownership and dispatch

QueueAdd saves a Pending item without starting it. QueueStart binds the exact saved
revision and explicit confirmation and starts one daemon owner for that session.
At most eight session owners can run. Each uses the existing eight-transfer global
limit and one active transfer per session, dispatching one item at a time.

Before calling the existing transfer port, persist Running with the exact predetermined
native transfer ID. The native job is protected from cache eviction until its queue
receipt is copied durably. Successful transfers commit their actual receipt, then
allow the next Pending entry. Any unconfirmed failure/cancellation or committed
cleanup failure pauses further dispatch and retains its Unknown/CleanupRequired
entry; the queue never retries or resumes it automatically.

The existing session transfer implementation remains the execution port, including
its advertised server-dependent publication behavior and cleanup reporting. This
slice does not claim P2 staged-file SHA/recovery semantics for that older port.
[ADR-0050](0050-verified-session-transfers-and-explicit-recovery.md) subsequently
adds a distinct verified queue mode and explicit recovery on the original live session.

An internal capacity/session refusal happens before transfer spawning; only that
known no-dispatch case can return the current item to Pending and pause the queue.
Unknown wire results are never treated as that refusal. Metadata failures stop the
owner; subsequent inspect conservatively marks orphan Running entries Unknown and
pauses. Merely loading/reconnecting never starts a queue.

QueuePause stops future dispatch and leaves the current transfer running. Current
transfer cancellation uses the existing exact session/native ID cancellation route.
Pending cancellation never sends a file operation. QueueRemove removes only queue
metadata and cannot remove a Running item or any file. Explicit continuation processes
only Pending items and does not replay prior Unknown/cleanup entries.

GUI disconnect does not cancel the daemon owner. A daemon restart does not rebind a
queue to a new session, automatically reopen credentials, replay Running entries or
start saved Pending paths. Old session queues remain inspectable/cancellable as
metadata; explicit session rebind is future work.

## Protocol and clients

Minor 43 / SFTP_BROWSER_QUEUE bit 47 adds SftpOperation LocalList (6), QueueGet (7),
QueueAdd (8), QueueStart (9), QueuePause (10), QueueCancelPending (11), QueueRemove
(12), and Conflict status (6). Existing request/response gain additive document JSON,
expected revision and confirmed fields. Empty/default fields retain legacy wire
shapes. Daemon routing and clients require the new capability for these operations.
New operations have strict canonical request shapes.

The desktop shares the existing background SFTP connection. Queue add/control
requires the displayed session/revision and writable authority, with separate
path/start/metadata-removal acknowledgments. Revisions and session changes clear
confirmations. The worker rechecks authority before sending queued mutations, verifies
reply session/next revision and disables writes after an unknown/rejected mutation.
Only explicit QueueGet reload restores authority; background polls cannot do so or
clear a waiting mutation's busy flag. Session changes clear old local/queue state.

CLI adds local-list/remote-list, queue get/add/start/pause/cancel/remove and exact
transfer get/cancel. Mutations require explicit --confirm and queue revision; requests
are sent once through authenticated daemon IPC. Paths remain daemon-local in the
same way as the existing SFTP control API.

## Deferred verification and remaining P3

This slice ran cargo fmt only. Unified verification must cover legacy wire shapes,
feature rejection, document bounds/redaction/checksum, file-lock/revision conflicts,
sync/rename/disk-full/corrupt storage, GUI scope epochs and lost replies, owner/capacity
cleanup, protected job eviction, FIFO ordering and pause races, current cancellation,
orphan recovery, reconnect/restart without replay, local links/non-UTF-8/limits,
directory truncation and actual external SFTP publication/cleanup behavior.

The remaining P3-010 coding listed in this historical slice is completed by
[ADR-0051](0051-complete-sftp-workspace.md): reviewed new-session migration, remote
editing, conflict policies, file management, concurrency, link policy and drag/drop.
P3-010 verification, P3-020 through P3-060 and P3/P4 gates remain in the original plan.
