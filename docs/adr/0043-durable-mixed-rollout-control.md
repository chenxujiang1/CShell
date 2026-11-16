# ADR-0043: Durable mixed batches, canary, gates and failure threshold

- Status: Accepted
- Date: 2026-10-10

## Frozen policy and compatibility

Extend P2-050 mixed orchestration with optional MixedTaskPolicy.rollout. It fixes
batch size (1–1,000), canary count (0–1,000, capped by selected target count), canary
approval, approval between batches, delay and optional positive failed-target limit.
Targets are ordered by the frozen Host UUIDs. A nonempty canary prefix is the first
batch; remaining hosts form fixed batches. Each host still executes its mixed
steps sequentially through original typed command/upload/download children.

No rollout policy means the existing scheduling path. New optional policy/state/
confirmation fields deserialize as None and are omitted when encoding, preserving
old ordinary mixed snapshot/row/start-document bytes and fingerprints. Plans with
rollout fields require the new capability; older strict decoders can reject them.
New-run retry keeps the frozen rollout policy, derives batches for its new subset
and initializes fresh control state, never carrying an old gate into the descendant.

## Durable carrier and CAS

Rather than a separate unbound control table, the first frozen host's existing
MixedTargetRun ledger carries optional MixedRolloutProgress for advanced plans.
Other hosts must have no control state. Carrier identity is frozen by the snapshot;
it remains a normal host with unchanged native receipt semantics. Control has its
own revision, Ready/Running/Paused/Stopped/Finished stage, next batch, approved batch
set, one-use gate, persistent not-before time, first-start total deadline and fixed
stop reason. The carrier target revision also advances on a control update.

Opaque MixedTargetAction::Rollout validates policy, carrier, metadata shape and
transition, retaining target/step state and every native receipt. It cannot modify
control while that host is Running. Writes use the existing snapshot-bound target
CAS and event transaction. Controller updates occur before dispatch or after joined
batch workers, when the carrier is stopped. Leaf repositories preserve control
fields; no synthetic leaf result or exit code is generated from orchestration state.
This uses schema 15 and existing rows/events with no migration or dependency update.

## Dispatch, pause and stop

Start binds the complete Pending Host/revision set, frozen snapshot hash and the
current control revision. The carrier's Pending revision, when included, must agree
with its control document. A gate, active owner, expired deadline or already reached
failure limit blocks start. Controller start is persisted before native dispatch,
then target rows are reloaded to include the carrier CAS revision. A restart resumes
from the earlier of the persisted batch and first remaining Pending batch, so a
completed canary whose gate transition was interrupted is reevaluated and cannot
be silently skipped.

Only one frozen batch is dispatched at once, within original host concurrency and
shared endpoint/jump/connection leases. Failure count uses actual Failed/Unknown/
TimedOut target rows across the run, not metadata stage or Cancelled rows. Reaching
the limit stops new dispatch and cancels in-flight work; cancellation preserves
native uncertainty. Canary must finish with every cohort host Succeeded before
advancing, even without an approval gate. Canary failure stops later hosts.

On user cancellation, total timeout, threshold or canary failure, unstarted targets
are explicitly closed without native dispatch (Cancelled, or TimedOut for total
timeout). This permits later reviewed new-run retry of eligible hosts and does not
silently resume them. Storage failures cancel/drain workers and preserve uncertain
durable intents for ordinary recovery if final control commit cannot succeed.

Batch delay is persisted when advancing. Approval gates are persisted with a fresh
UUID, kind and next batch; entering Paused returns the runner and releases its run
owner. Approval binds exact control revision/gate and consumes it once. Approval only
sets Ready and records that batch approval; it does not launch a runner. The user
must review a fresh full Pending/control confirmation and explicitly start again.
Approval and stop do not replace stored file receipts or bypass file repair rules.

The total deadline starts on first rollout start and includes delay/approval waiting;
it is never reset by approval, explicit continuation or restart. Batch queue timeout
starts when that batch is eligible. Cancellation/deadline observations are cooperative
around dispatch/waits and existing child ports; a request already on the wire remains
subject to the existing Unknown rules. Native OS/server completion cannot be inferred
from a stop request.

Readonly inspect reconciles orphan Running targets and orphan Running control to
Stopped/Restarted without issuing SSH/SFTP. It does not generate approval, discard a
gate, move onto a later batch or restart a command automatically. Explicit cancel of
an idle/paused parent closes its Pending targets and clears the gate with a stop event.

## Protocol and clients

Minor 39 / MIXED_ROLLOUT bit 43 adds MixedRolloutGet (55) and Approve (56), requiring
TASK_CONTROL and MIXED_TASK_CONTROL. Get accepts only the run ID; Approve additionally
requires nonzero control expected_revision, gate_id and confirmed. Canonical shapes
reject unrelated fields. Reply is a separate `cshell.mixed-rollout` v1 document with
carrier/target revision, control revision/state, frozen policy/hash, counts and active
owner. Output and top-level revision fields remain empty/zero.

MixedStartConfirmation optionally adds control_revision; advanced start requires it,
legacy start omits it. Ordinary MixedGet/Review retain their outer document versions,
with the optional policy/carrier state only for advanced plans. Client code decodes
and checks control identity/policy/carrier against the frozen plan. New Create and
advanced Start negotiate the rollout feature before requests, while ordinary plans
continue using the base feature. Other codecs continue to validate optional policy
and carrier metadata rather than treating it as a native success result.

GUI drafts include rollout enable/size/canary/gates/delay/failure controls and explain
deadline semantics. Loaded advanced plans query control separately, show policy,
stage/reason/deadline/gate, and disable start without a valid Pending/control binding.
Gate confirmation resets on control revision change/deactivation. Background approval
is a mutation under the existing uncertain-reply authority epoch; it validates the
consumed gate/new revision, refreshes status and does not start another batch.

CLI adds mixed-rollout-get <run-id>, mixed-rollout-confirmation <run-id> and
mixed-rollout-approve <control.json> --confirm. The confirmation helper combines
control and status and refuses inconsistent versions/active owners/pending gates.
The resulting JSON is used by the existing mixed-start command. Approval prints
updated control for review and requires a separate start. Requests authenticate,
check capabilities/reply identity/shape, and are never automatically replayed.

## Implementation status and remaining work

Code and source formatting only per the user. Compilation/tests/audit/MSRV, native
GUI/CLI/IPC, migration, real SSH/SFTP and daemon replacement remain deferred. The
current daemon/live database/package-lock.json stay untouched; no P2 gate closes.

Consolidated verification must cover legacy serialization/fingerprints, optional
state/carrier placement, deterministic batches/small subsets/canary truncation,
single-cohort success, threshold/cancel/timeout/drain and unstarted non-dispatch,
carrier/native CAS interactions, source retry provenance with control metadata,
delay/deadline persistence, approval nonce/revision replay, crashes before/after
batch/gate transitions and orphan recovery, cross-page start binding, client feature
and epoch gates, and original four-platform/1,000-host filesystem/server matrices.
Check/template steps, advanced command editing, quick-command task launch, aggregate
index/search/text diff, broader file conflict/retention policies and remaining P2
code plus unified P2-099 verification are still open.
