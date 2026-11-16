# ADR-0037: Immutable mixed result captures and client export

- Status: Accepted
- Date: 2026-10-10

## Capture boundary

Add `cshell.mixed-results` v1 for a stopped mixed run. Capture requires every
parent target to be outside Pending/Running and no active execution or repair
owner. Unknown remains a stopped observational result, never inferred success.
The mixed service holds its operation lock through orphan reconciliation, reading
the persisted snapshot/targets and constructing the result. A later repair or start
cannot change that captured document. Interrupted capture has no native side effect.

The header binds the frozen run UUID/SHA-256, Host/Profile catalog revisions,
policy, step metadata, creation/capture times and complete target count. Rows retain
frozen host identity/address/port, parent and child UUIDs/revisions, cursor, actual
command statuses/exit codes/times/reasons and native upload/download receipt data.
Unstarted suffix steps remain Pending in a stopped parent. Command bodies,
variables/environment, authentication configuration, local/remote paths and stdout/
stderr bodies are absent. Names and host metadata may still identify the workload.

Seven-state target/step totals, actual command exit-code and reason counts, file
progress, verified/committed/adopted counts and committed temporary cleanup counts
are derived from the same rows. Outcome groups compare statuses, cursor, command
exit/reason data and file phase/intent/progress/content/ownership/receipt flags.
Identity, revisions and timing do not determine grouping. Group equality is not an
output comparison or evidence of identical native execution.

## Bounded protocol

IPC minor 33 / MIXED_RESULTS bit 37 adds MixedResults (41) and MixedResultPage (42),
also requiring TASK_CONTROL and MIXED_TASK_CONTROL. ResultPage additionally carries
`result_id` bytes at TaskRequest tag 19, an optional Host cursor and limit 1–64.
Canonical requests reject irrelevant fields. Existing mixed status and repair
documents are unchanged. No dependency version or SQLite schema change is required;
CLI promotes its existing domain dependency to the normal dependency set.

Capture returns a `cshell.mixed-result-manifest` v1 with a separate result UUID,
header and SHA-256 of the complete canonical compact result document. Server-side
captures are immutable, ephemeral metadata held in a cache of at most eight entries
and 128 MiB of canonical serialized data. Each document is limited to 64 MiB, and
tokens expire after ten minutes; expired entries are removed on cache access.
Capacity evicts the oldest expiration. Restart drops all captures. This cache is
not durable raw-output archival storage. Expired/evicted tokens fail with Missing,
and clients explicitly capture again rather than silently mixing new rows.

Pages read the stored capture, not live target rows, contain at most 64 targets and
12 MiB, and bind run/result UUIDs, result digest, cursor and limit. Manifests are
bounded to 256 KiB. A shared application collector limits aggregate received page
bytes to 64 MiB, checks strict cursor/count progression, requires the complete set,
validates native row structures and cross-page child identity uniqueness, derives
summary/groups again and verifies the complete SHA-256. Desktop also checks every
row against its already loaded frozen plan. Collection failure publishes no partial
result. Protocol bounds are explicit; very large results may be rejected.

## Clients and export

Desktop capture is explicit and performed in the background. It retains one complete
immutable result, displays captured target state/group filters, actual exit/reason
counts and file receipts, and clearly labels it separately from the live status page.
Changing the selected run clears the old result. Later repairs in the same run leave
the result unchanged; recapture replaces it. Export confirmation binds the current
document object, format and local path, resets on changes/deactivation, and exports
all captured targets regardless of either result filters or live-page filters.

CLI adds `mixed-results <run-id>`, `mixed-result-page <manifest.json> [after-host-id]`
and `mixed-export <run-id> <json|csv> <absolute-new-file>`. The first prints a manifest
for explicit saving, the second reads a page of that same token, and export captures,
collects and verifies all targets before writing. Authenticated replies are checked
for capability, request/run identity and absence of unrelated output/revision fields.
Requests have deadlines and are never automatically retried.

GUI and CLI share a bounded streaming formatter and the existing client-side export
publisher. Paths never go to daemon. JSON includes the complete metadata document;
CSV has one host/step row with actual command codes (empty for file steps), native
file intent/progress/hash/ownership/verification/commit/adoption/cleanup fields and
parent/child identity/revision binding. Text cells reuse formula-prefix protection
and quote/control escaping. Export creates a new absolute destination through staged
sync and publication without replacing an existing file. Failure requires inspecting
the destination before a new explicit attempt. It grants no execution authority.

## Status and deferred verification

Code and source formatting only, per the user's instruction. Compilation, tests,
migrations, native GUI/CLI/IPC, actual SSH/SFTP and filesystem publication are deferred
to consolidated verification. The live DB, existing daemon and original package lock
remain unchanged. Existing verification does not cover this WIP and no P2 gate closes.

Consolidated verification must cover stopped/active/Unknown capture eligibility,
start/repair/cancel races, orphan recovery, future suffix states, exact exit codes and
receipt counts/groups, cross-page identity and summary/digest tampering, cursor/count/
byte bounds, TTL/eviction/restart, capabilities and response identities, CSV text and
empty fields, new-file publication failures/races and GUI confirmation changes.
Full output hash/diff/search/merge and durable raw archive, batch/canary/approval/
threshold/check/template/new-run retry, backup/replacement policies and the remaining
Phase 2–4 implementation and platform matrices continue separately.
