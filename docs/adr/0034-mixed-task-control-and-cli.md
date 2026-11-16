# ADR-0034: Mixed task control, bounded status and CLI

- Status: Accepted
- Date: 2026-10-09

## Control protocol

Expose the existing mixed-task service over authenticated task IPC. Minor 31 and
MIXED_TASK_CONTROL bit 35 add MixedCreate/Get/List/Review/Start/Cancel/Output,
operations 29–35. All operations also require TASK_CONTROL. Existing clients and
standalone command/file operations keep their request shapes. Canonical requests
reject unrelated fields, including the new expected_targets_json (tag 18) outside
MixedStart. Request buffers are zeroized on drop; diagnostics omit their bodies.

Create validates the typed definition, selection and Host/Profile revisions under
the catalog lock, freezes targets once and saves the existing schema 15 ledger. It
returns the complete frozen snapshot for explicit review and does not start SSH or
file transfer. Review separately returns that snapshot. Neither contains live Vault
secrets; command configuration and authentication-template metadata are only in
the explicit plan/review document, not ordinary status/list replies.

## Bounded status and start confirmation

MixedGet uses an exclusive Host ID cursor and a limit from 1 to 64. SQLite reads
the snapshot, a bounded page, aggregate counts and the complete Pending Host/revision
list in one transaction. It does not deserialize every large target row to produce
each page. Lists use an exclusive run ID cursor with at most 64 compact summaries.
Orphan recovery remains metadata-only and runs only without a live parent owner.

`cshell.mixed-status` v1 contains immutable snapshot SHA-256, revisions, policy,
step name/type/success-code or content-limit information, aggregate target counts,
the complete Pending revision list, cursor and bounded target rows. Rows include
native typed receipts and scoped child IDs. Commands, variables/environment,
credential templates, paths and output bytes are omitted. Client decoding checks
identity, pagination, counts, ordering, time/receipt bounds and successful prefixes;
the storage-side full snapshot validator remains authoritative for native transitions.

The status active flag is a live ownership hint; it does not replace durable receipts.
Each page is a separate observation of a running job, not a cross-page atomic result.
The Pending set comes from the same transaction as that page's counts. A
`cshell.mixed-start` v1 confirmation binds run ID, saved snapshot SHA-256 and every
Pending Host ID/revision. It is bounded to 1,000 entries/128 KiB, rejects duplicates,
unsorted/nil identities and invalid versions, and requires explicit confirmation.
The daemon rechecks the whole current Pending set and snapshot fingerprint before
launch; it cannot silently widen a subset or accept a stale confirmation. The start
acknowledgement confirms launch, not successful execution. Cancel bypasses the normal
task-operation lock and never deletes a file or replays a child mutation.

## Scoped output

MixedOutput binds parent, frozen Host ID and mixed step index. Only a dispatched
command step can read output. The server derives its child run UUID and native step
zero; callers cannot supply another child UUID or use file steps as command output.
Each read is at most 64 KiB from the existing protected stdout/stderr store. Missing
output is unavailable rather than fabricated empty success. Invalid ranges are
rejected. An optional exact target revision is checked before and after the byte
read; zero requests latest readonly inspection. The context echoes scope, revision,
recorded step state, offset, next offset and observed length. Live pages do not claim
complete output, atomic metadata/bytes or a final digest.

## CLI

Add task mixed-create/review/get/list/confirmation/start/cancel/output. Create checks
the returned definition, original selection and both revisions. Get/list check run,
cursor, limits and typed data; output checks scope/context before printing JSON bytes
or explicitly requested raw bytes. Confirmation reads status and prints the complete
bound Pending set without executing. Start requires a saved confirmation document
and --confirm. All mutations are sent once; a lost reply prompts state inspection.

Typical sequence (using user-prepared definitions and selection documents):

```text
cshell-cli task mixed-create definition.json selection.json <host-revision> <profile-revision>
cshell-cli task mixed-review <run-id>
cshell-cli task mixed-get <run-id>
cshell-cli task mixed-confirmation <run-id>
cshell-cli task mixed-start confirmation.json --confirm
cshell-cli task mixed-get <run-id> [after-host-id]
cshell-cli task mixed-output <run-id> <host-id> <step> stdout 0
cshell-cli task mixed-cancel <run-id>
```

Save the confirmation command's JSON output to confirmation.json using the caller's
chosen shell/file workflow. Mixed leaf definitions require concurrency one; command
leaves contain one step with nested gates/batches disabled. Parent concurrency and
ordered execution remain as defined by ADR-0033.

## Status

Code only, per the user's instruction. Only source formatting is performed, with no
compilation, tests, migration or live IPC/SSH/SFTP actions. No dependency or schema
version change is added in this iteration. GUI mixed editing/control, embedded file
repair, richer results/export, canary/batches/check/template steps and the full native
and failure matrix remain later work. Existing daemon, live DB and package-lock.json
remain untouched; old verification evidence does not cover this WIP.
