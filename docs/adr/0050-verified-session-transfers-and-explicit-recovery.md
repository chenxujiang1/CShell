# ADR-0050: Verified session transfers and explicit recovery

- Status: Accepted
- Date: 2026-10-12

## Scope and compatibility

Continue P3-010 by adding verified upload/download execution to the durable session
queue from ADR-0048. The existing live SSH session is the execution authority;
credentials are not resolved again and a dead session is never silently replaced.
This slice implements code only. Compilation, tests, native GUI and actual transfers
remain deferred. [ADR-0051](0051-complete-sftp-workspace.md) subsequently completes
the P3-010 coding for reviewed new-session migration, remote editing, file operations,
configurable link/conflict policy, concurrency and rate limits. Its managed execution
extends the no-replace default described here; this ADR records the original slice.

Draft gains a default-false, omitted-when-false verified flag. Item gains an optional,
omitted-when-absent SftpVerifiedTransfer journal. Receipt gains optional bounded detail.
Legacy documents therefore retain canonical serialization/checksum when those fields
are absent. Queue/storage versions remain 1, with strict shape validation. There is
no SQLite migration or dependency upgrade. New fields are not silently discarded by
older clients; old QueueStart refuses Pending verified entries. Legacy direct
Upload/Download and legacy queue entries retain their existing execution port.

## Journal and execution

The journal freezes full source size/SHA-256 before temporary creation, then records
Create, Write, Publish and Cleanup intents separately from acknowledgments. It keeps
confirmed bytes, the SHA-256 of that prefix, an optional outstanding write boundary,
temporary ownership, full verification, confirmed publication and confirmed cleanup.
The temporary name is derived from the queue item ID beside the destination; recovery
requests never supply another cleanup path. Shapes, 64 GiB source limit, write bounds,
receipt/journal consistency and final state prerequisites are validated on load/save.

Initial dispatch persists Running and exact native transfer ID before spawning.
Verified and legacy jobs share the existing global eight-transfer and one-per-session
budget/cache protection. Queue owners remain limited to eight. Journaling takes the
session queue lock, reloads current metadata, checks item/draft/transfer ownership and
the previous journal, preserves other queue changes, and saves by revision CAS.
No native file mutation occurs if its prior intent cannot be saved.

Uploads open an absolute regular local source, reject parent links/reparse points,
pin a handle, compare metadata/identity, and hash the full content. Downloads use
the existing remote PinnedRead handle and full digest. Sources are opened at dispatch;
the path saved when adding an entry does not freeze the contents at that earlier time.

Upload uses StagedUpload, requiring advertised hardlink/fsync support before writing
the temporary; unsupported servers never fall back to legacy publication. Download
uses LocalDownload through an explicit session-path constructor. A 32 KiB chunk is
preceded by a durable Write intent. Write and file fsync finish before the confirmed
offset/prefix digest is saved. Source and temporary full hashes must match before
verified=true is saved. Publish intent precedes a no-replace hardlink. Its successful
acknowledgment is saved as committed before cleanup begins. The published target and
temporary are probed again before cleanup; Cleanup intent precedes the exact temporary
removal and cleaned is saved afterward. Existing destinations are never overwritten.

The worker has a 24-hour execution deadline, cancellation polling, a thirty-second
SFTP open deadline and a two-second graceful close allowance followed by forced SFTP
channel closure. Closing a channel does not remove files or infer ownership. Queue
status includes durable progress even when recovery keeps the queue paused. Native
terminal status uses the last readable durable facts; storage/transport uncertainty
never manufactures a commit receipt. Failed/cancelled work retains the temporary and
pauses the queue. Inspect converts orphan Running to Unknown, without dispatch.
The queue's terminal collector reloads the journal before saving its receipt, including
when a cancelled blocking metadata save finished after the worker's memory view.
Durably committed and cleaned work remains successful if cancellation arrives afterward.

## Explicit recovery

Recovery requires a paused queue without an active owner, the exact saved revision,
explicit confirmation and one eligible item. It persists Running with a fresh native
transfer ID, obtains the same shared transfer budget, and operates only on that item.
Other Pending entries remain paused. A known pre-spawn capacity/session refusal restores
the previous item metadata; an uncertain operation never rolls back into Pending.

- Resume requires confirmed temporary ownership, an uncommitted, uncleaned journal,
  a saved source hash and no outstanding Publish/Cleanup. The new source full digest
  must match. The destination must be absent. Actual temporary size must lie between
  the confirmed offset and the saved outstanding write end (or equal the offset when
  no write is outstanding). Reopen is checked; every temporary prefix byte is compared
  with the pinned source and the probe digest before adopting the actual offset.
  Only then can append, full verification and publication proceed. A changed, missing,
  shorter or out-of-bound temporary is refused without truncation or replay.
- Adopt requires a previously verified Publish-unknown or orphan committed journal.
  Read-only probes must find the destination matching the saved full size/SHA, and
  any remaining temporary must match too. This records matching content, not proof
  that this client created an indistinguishable existing target. It never republishes
  or overwrites. Missing temporary is recorded cleaned; an existing one stays for a
  separate explicit cleanup action.
- Cleanup requires confirmed ownership and excludes unconfirmed publication. The
  destination must match a known commit or remain absent for uncommitted work. The
  temporary must match a saved prefix digest, or its bounded uncertain tail must be
  compared with a newly hashed matching source. That inspected prefix is persisted
  before removal so an uncertain cleanup can be inspected again. Missing temporary
  is recorded cleaned. Cleanup never removes the destination. Cleaning uncommitted
  work retains Unknown metadata; it does not create a successful-transfer receipt.

An unconfirmed Create has no acknowledged ownership: automatic resume/cleanup/adoption
is not offered, and the planned path is visible for inspection. A lost Publish reply
only offers explicit matching-content adoption. Source changes never authorize a
restart. No operation automatically reconnects or rebinds another SessionId. A daemon
restart therefore leaves old session queues inspectable but requires a later reviewed
session migration feature before their native recovery can run.

## Protocol and UI

Minor 45 / SFTP_VERIFIED_QUEUE bit 49 adds QueueAddVerified (17), QueueStartVerified
(18), QueueResumeVerified (19), QueueAdoptVerified (20), QueueCleanupVerified (21).
All require SFTP_CONTROL and SFTP_BROWSER_QUEUE as well. Add validates that the draft
mode matches the operation. Start/repair use exact revision and confirmed=true;
repair uses the item ID in transfer_id. New canonical requests reject unrelated fields.
Directory capability checks remain restricted to operations 13–16.

Desktop new queue entries use verified mode. It shows legacy/verified mode, source
SHA, durable offset, intent/commit/cleanup facts, acknowledged or planned temporary
path, and persisted terminal detail. Separate explicit recovery acknowledgment and
eligible per-item buttons select resume/adopt/cleanup. Session/revision changes clear
that acknowledgment. IPC mutations retain the existing authority checks and no-replay
behavior. CLI adds queue-add-verified/start-verified/resume/adopt/cleanup with revision
and --confirm; queue-get emits the validated journal. Older daemons reject the new
feature instead of executing an alternative transfer mode.

## Deferred verification and limits

Only cargo fmt ran. Unified verification must cover old JSON/checksum bytes and wire
features; empty/short/large files; unsupported hardlink/fsync; all intent/ack loss and
storage-failure boundaries; cancellation during hashing/write/fsync/publish/cleanup;
prefix rechecks and changing sources; partial writes; resume without duplicate IDs;
capacity rollback; other queue mutations during journaling; paused recovery polling;
orphan/restart behavior; GUI acknowledgment/scope; Windows/Unix file semantics and
actual external SFTP servers. Local/SFTP name-based probe-then-publish/delete races
remain subject to the existing staged-port limitations and native verification;
these operations do not claim a server-side compare-and-swap or immutable filesystem.
Per-chunk durable metadata/file sync is intentionally conservative; throughput tuning
must preserve the recorded intent/ack boundaries after unified verification.
