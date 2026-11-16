# ADR-0042: Explicit new-run retry of stopped mixed targets

- Status: Accepted
- Date: 2026-10-10

## New run versus existing file recovery

Add a mixed-task retry operation that saves a new Pending run for explicitly chosen
stopped, unsuccessful hosts. It does not start the run, continue the original cursor,
copy native receipts into new children, clean old artifacts or change the source
run. Execution requires the existing full Pending-set confirmation and separate
start after reviewing the new frozen plan. Every selected host begins at step zero.

Confirmation binds the source run UUID/frozen snapshot SHA-256 and a sorted unique
Host/revision set (1–1,000). Pending/Running/Succeeded targets are rejected. The
source service excludes an active execution/repair owner and checks source rows
after ordinary orphan reconciliation. All prior command dispatch intentions (native
step started_at_ms is present), including successful prefixes and failed commands,
require an idempotent declaration in the original frozen command definition.
Unstarted commands may retain their original declaration because they are not
being replayed. The user cannot edit that declaration through this retry operation.

Unknown parent results additionally require an explicit acceptance of possible
duplicate command effects. Any Unknown or active native file receipt blocks new-run
retry regardless of that flag: use original parent-bound file repair first. A plan
containing files also requires explicit whole-plan file replay acceptance. The new
run uses fresh temporary ownership and the original paths and upload source digest;
it does not resume or adopt old file receipts. Known committed destinations may cause
the new no-replacement file step to fail, old temporary files remain owned by the
original run, and download sources are reread in the new run. This interface does
not silently skip a successful file or replace an existing destination.

## Frozen subset and provenance

The new definition/policy and selected frozen host/connection/credential closure are
copied exactly from the source snapshot. Current Host/Profile catalogs are not used
to rebind the retry. A fresh target-snapshot UUID, parent UUID and every child UUID
are created; none may equal the original parent/child matrix. Initial target rows
and leaf ledgers are fresh Pending at step zero.

Ordinary mixed snapshots stay `cshell.mixed-task` v1 with no new serialized field;
missing retry data deserializes as None and is omitted when encoding, preserving v1
bytes/fingerprints. New retry snapshots use v2 and required MixedRetryProvenance:
parent UUID/SHA-256, selected Host/revision/state/receipt-document SHA-256 checkpoints,
the union of replayed command indexes and recorded Unknown/file acknowledgements.
Shape validation rejects v1 with provenance or v2 without it. The hash commits all
provenance, while native child execution remains the existing typed runner behavior.

Receipt documents are not copied into the new snapshot, keeping provenance bounded.
The current checkpoint is digest-bound when the new run is created; later original
repairs do not rewrite it or the descendant. Snapshot v2 still uses the existing
12 MiB limit, so an oversized frozen plan plus provenance is rejected.

## Atomic repository insertion

MixedTaskRepository create now requires retry-aware insertion. SQLite loads the
immutable parent snapshot and each selected source row inside the same transaction
that inserts the new snapshot, initial targets and initial events. It checks frozen
definition/targets/closure, new identities, exact source revisions/states/document
digests/timestamps and replay eligibility/command-index union before any insert.
A stale source checkpoint fails; it is never rebased or silently dropped from the
selection. A failure rolls back the entire new run. The original rows/events are
not mutated by retry insertion. Existing SQLite schema 15 needs no new columns.

## IPC and CLI

Minor 38 / MIXED_TASK_RETRY bit 42 adds MixedRetry (54), also requiring TASK_CONTROL
and MIXED_TASK_CONTROL. The request carries the source UUID, confirmed flag and
`cshell.mixed-retry` v1 in expected_targets_json, with selected revisions and separate
Unknown/file acknowledgements. Canonical shapes reject unrelated fields. Reply is
the new UUID and full frozen v2 snapshot; revision/output fields remain empty/zero.
The service distinguishes plan limit, stale state, unsafe request, clock and storage
failures. There is no new protobuf field, package/dependency version or migration.

CLI mixed-retry-confirmation <review.json> <status-page.json> <host-id[,host-id...]>
[--confirm-unknown] [--confirm-files] validates a bounded review/status pair, checks
the selected rows and prints a confirmation for explicit saving. This helper selects
only hosts present in the supplied status page; cross-page confirmations may be
assembled explicitly, subject to the same sorted-set/backend checks. Review/status
input is bounded to the existing 12 MiB snapshot limit; confirmations are 128 KiB.
mixed-retry <confirmation.json> --confirm authenticates/capability-checks, reviews
the immutable source once, sends creation once and checks returned parent/selection/
acknowledgements/new identities/frozen definition before printing the saved new run.
An interrupted/unknown creation reply requires inspecting saved runs; no mutation
is automatically replayed.

## Desktop

The mixed panel provides a separate retry selection and confirmation section. It
explains step-zero replay and old file ownership/destination behavior, labels blocked
targets and keeps each selected Host's exact version across pages. Selection is
independent of the ordinary current-page filter. Returning to a page with a changed
or now ineligible selected row removes that selection; off-page stale revisions are
left for the server's exact transaction check. Visible selected host/name/version/
state entries make the creation request reviewable.

Run/hash changes clear selection; page/control changes and deactivation clear all
retry acknowledgements. Page identity uses the existing Arc, avoiding full status
serialization every UI frame. Creation is disabled while busy, cancelling, inactive
connection, blocked write epoch or active parent. The background operation is marked
as a mutation, so uncertain send/decoding/follow-up replies retain the existing
authority epoch barrier. It validates the new snapshot provenance/target subset and
loads its status without starting it. The plan view displays the parent linkage and
acknowledgements. Subsequent start uses the new full Pending confirmation.

## Status and deferred verification

Implementation and source formatting only per the user. Compilation/tests/audit/MSRV,
SQLite migrations/native GUI/CLI/IPC, actual SSH/SFTP and daemon replacement remain
deferred. Original daemon/live database/package-lock.json are untouched; existing
proof does not cover this WIP and no P2 gate closes.

Consolidated verification must cover v1 byte/fingerprint compatibility and v2 shape,
exact frozen closure/subset/new UUID matrix, known versus Unknown command intentions,
successful-prefix/nonidempotent/file-uncertainty rejection, file acknowledgements and
no replacement/cursor adoption, source revision/digest/timestamp/definition tampering,
atomic rollback and concurrent source repair/start, creation-ACK loss and epoch rules,
cross-page selection/confirmation changes, CLI helper limits and new-run non-dispatch.
Mixed batch/canary/approval/threshold/check/template features, advanced command forms,
archive/aggregate/index work and remaining Phase 2–4 implementation/gates continue.
