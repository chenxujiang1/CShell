# ADR-0036: Parent-bound mixed file repair

- Status: Accepted
- Date: 2026-10-10

## Scope and binding

Add explicit file Probe/Resume/Cleanup/Adopt and status to mixed tasks. Requests bind
parent run, frozen Host ID, mixed step index and exact parent-target revision. The
server derives the original child UUID, leaf snapshot and revision; clients cannot
substitute another child or standalone run. Commands cannot use this interface.
Mutation is limited to the current stopped file step. Readonly probe may inspect
already dispatched current/prefix file steps; never an unstarted suffix.

BeginFileRepair is an opaque CAS transition from a stopped target to its current
file step's Running intent. The original leaf ledger remains in place and keeps its
temporary ownership, prefix progress, content, publication intent and receipts.
Private child repositories allow readonly inspection but reject every write through
that mode. Mutating repair uses the existing upload/download runners and original
frozen credential/host-key/local filesystem ports, shared owners and connection budget.

Resume requires known temporary ownership, unverified content and no uncertain
publication. It retains the original full-prefix/content checks. Cleanup only touches
the owned temporary name and never removes the final file. Adoption requires the
most recent completed probe to match the same parent revision, host, step, child ID
and leaf revision, plus pinned size/SHA-256. A separate confirmation accepts the
unknown publication attribution. The native leaf runner rechecks actual content
before recording adoption; matching probe content is not a publication receipt.

## Completion and cancellation

FinishFileRepair reconciles the persisted native receipt. A successful leaf closes
only that step, preserves its successful prefix, and returns the parent target to
Pending at the next unstarted step (or Succeeded if it was the last step). It does
not execute a later command. Continuing requires a newly reviewed full Pending
confirmation and explicit start. Failed/Unknown/cleanup-required receipts retain
their distinct native meaning and do not manufacture a success or exit code.

An uncertain storage/native result leaves durable Running/leaf intent for ordinary
orphan recovery rather than replaying a mutation. Readonly recovery can reconcile
known success or mark uncertainty, but cannot issue native I/O. A daemon-owned
repair job shares the existing parent active map, so normal start and another repair
cannot run concurrently in that parent. Cancel reaches the same token and reduces
authority without deleting artifacts. Native operations use the existing deadlines
and attempt owned channel cleanup before releasing their leases.

Jobs retain at most 64 latest entries; active entries are not pruned. Each job records
bound parent/child identities and start revisions, operation, Running/Completed/Failed,
optional probe, observed result revision and fixed error category. Job hints are
ephemeral; durable parent/leaf rows remain authoritative after restart. Completed
means the job returned, not that every repair succeeded or the parent may auto-run.

## IPC and clients

Minor 32 / MIXED_FILE_REPAIR bit 36 adds MixedFileProbe/Resume/Cleanup/Adopt/Status,
operations 36–40, also requiring TASK_CONTROL and MIXED_TASK_CONTROL. Probe and writes
require nonzero expected_revision; writes require confirmed. Canonical shapes reject
unrelated fields. Launch acknowledgement returns immediately; status reads the
separate `cshell.mixed-file-repair` v1 document. Ordinary MixedGet shape is unchanged,
so earlier mixed status decoders do not receive new fields. Schema 15 and dependency
versions are unchanged; repairs reuse existing opaque target patches and events.

CLI adds mixed-file-probe/resume/cleanup/adopt/status. Indexes are zero-based, parent
revision comes from mixed-get, mutation flags are explicit, and replies are typed and
identity-checked. GUI adds per-file probe, stopped-current-step action selection,
separate confirmation and repair-job status. Adoption is gated by the currently
matching probe. Changing target/operation/page/control/job clears confirmation;
background polling reads status only and never repeats a launch. Unknown replies
retain the existing authority epoch rule. Base paging remains independent until a
repair status has explicitly been requested or a repair initiated.

## Status

Implementation and source formatting only, per the user's instruction. No compilation,
tests, migrations, GUI/native operations, daemon replacement or actual SSH/SFTP/hash
activity is performed in this iteration. Existing daemon/live DB/package-lock.json
remain untouched. Consolidated verification must cover scope/revision/capability
gates, adoption observation/attribution, cancellation and storage-loss checkpoints,
restart without job hints, later-step non-dispatch and the existing server/filesystem
matrix. Larger result/export/archive, batch/canary/check/template and remaining
Phase 2–4 work continue; no overall gate is closed.
