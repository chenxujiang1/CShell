# ADR-0038: Background full stored-output digest and byte comparison

- Status: Accepted
- Date: 2026-10-10

## Observation scope

Extend P2-060 beyond the existing 64 KiB sample comparison with a readonly job that
hashes one complete stored stdout/stderr stream, or hashes and compares the same
step/stream for two hosts in the same ordinary or mixed run. Full means all bytes
in the daemon's local output file, not a guarantee that every remote byte arrived.
Unknown stays Unknown and no analysis result changes task status, grants repair
authority, retries a command or creates an SSH connection.

`cshell.output-analysis-request` v1 binds run kind/UUID, step, stdout/stderr, left
Host/revision and optional right Host/revision. Ordinary tasks use the exact run
revision on both sides, require a terminal run without an owner, and require actual
terminal target and step states. Mixed tasks use each parent-target revision and
require no active parent owner and no Running selected row; only native terminal
command receipts are eligible. Already stopped command prefixes can be observed
while a mixed parent has a Pending later step. Files and unstarted command suffixes
cannot use analysis. The server derives the frozen command child UUID/step rather
than accepting arbitrary filesystem or standalone child references.

Bindings retain the frozen snapshot SHA-256, original child UUID/step and actual
native target/step states. Later mixed repairs/start may change parent metadata or
run later steps, but cannot dispatch a completed command child again. The observation
keeps its initial revisions and binding; it is not a refreshed execution receipt.

## Stored-file worker

The output store opens local typed-ID paths with private-directory checks, rejects
symlink/reparse/nonregular/oversized files, and holds read handles. Unix uses
O_NOFOLLOW/O_NONBLOCK; Windows opens reparse points for rejection and permits shared
read only, excluding new writer/delete handles while analysis holds the file. File
length/modification data and Unix device/inode/change times are checked against
opened handles and final path metadata. These checks depend on the private output
directory and platform behavior; the native replacement/permission matrix remains
part of consolidated verification.

At most two workers run globally; each uses two 64 KiB buffers and existing per-stream
1 GiB limits. It streams SHA-256 across all saved bytes, including empty streams,
and compares bytes at the same offsets without decoding or normalization. Results
include both lengths/hashes, exact equality, the number of unequal aligned positions
plus the excess length of the longer stream, and the first unequal byte offset.
This is a byte comparison, not a text edit script or line alignment. Missing output
is a distinct failure, never substituted with an empty successful stream. Truncation,
replacement/change observations, storage errors and invalid paths discard results.

Progress uses bounded atomic byte counters. Cancellation is checked before opening,
between chunks and before completion. The five-minute cooperative deadline is
checked at those points; an OS call may delay cancellation/timeout observation.
Blocking worker ownership retains its semaphore slot until native reads finish.
No detached replacement worker is launched after a timeout or uncertain reply.

## Jobs and protocol

IPC minor 34 / TASK_OUTPUT_ANALYSIS bit 38 adds OutputAnalyze (43), OutputAnalysisGet
(44) and OutputAnalysisCancel (45), requiring TASK_CONTROL and, for mixed clients,
MIXED_TASK_CONTROL. Analyze carries the typed request in definition_json with the
same top-level run UUID. Query/cancel carry the run UUID and opaque job UUID in
result_id. Canonical shapes reject every unrelated field. No new wire field,
dependency version or SQLite schema is required; existing mixed/result operations
and the sampled comparison remain unchanged.

Launch binds metadata and immediately returns a `cshell.output-analysis` v1 job.
States are Running/Completed/Failed/Cancelled; only Completed contains a result,
only Failed has a fixed error category. Query/cancel use a direct bounded metadata
path and do not wait behind task storage operations. Cancel only signals this local
analysis job. Completion may win a cancellation race and returns its existing result.

At most 32 jobs are retained; active jobs are never evicted. Completed/failed/cancelled
jobs expire after ten minutes and oldest finished jobs are evicted under capacity
pressure. Expired entries are removed on access. Restart drops all hints and cannot
resume or repeat a job automatically. Documents are bounded to 64 KiB and contain
identifiers, states, revisions, hashes/counts/offsets, but no output bodies, command
or credential data, host display names or local file paths.

## Clients

One independent background desktop connection is shared by ordinary/mixed panels
and windows, retaining one current job. Each panel selects a stopped command target,
can hash it or choose another eligible loaded target to compare, and displays the
job's explicit original scope/revisions and progress/result. Ordinary selection
uses loaded results; mixed selection uses the current target page. Running jobs are
polled readonly at bounded intervals. Explicit status and cancellation are available;
missing/expired jobs clear the stale active hint so a new explicit job can be launched.
The client checks request identity, original child UUIDs/snapshot hash, immutable
bindings, nondecreasing progress and stable terminal documents. Launch is never
automatically retried.

The first-difference button reads the corresponding existing 64 KiB output page
through the current task/child path and LogSurface. It requires the current loaded
scope/version/child binding; when the difference is the left stream's EOF, it opens
the longer right stream. It does not load entire outputs into GUI memory.

CLI adds output-analysis-request to print a typed request for explicit saving,
output-analyze <request.json>, output-analysis-get <job.json> and
output-analysis-cancel <job.json>. Indexes are zero-based, authentication/capabilities
and response identity/shape are checked, and query/cancel validate the original
request/bindings/progress. Job JSON can be saved explicitly as metadata; raw output
archival is a separate remaining feature.

## Implementation status

Code and source formatting only under the user's deferred-verification instruction.
No compilation/tests/audit/MSRV, migrations, native GUI/CLI/IPC, daemon replacement,
real output hashing or SSH/SFTP/file publication is run in this iteration. Existing
daemon/live database/package-lock.json remain unchanged and no P2 gate is closed.

Consolidated verification must cover empty/missing/binary/mismatching-length files,
first/last/chunk-boundary/EOF differences, full SHA-256 and byte counts, Unknown and
nonterminal/suffix/file rejection, ordinary/mixed revisions and child derivation,
later-step and repair races, progress/cancel/timeout/slot retention, TTL/eviction/
restart, platform permissions/reparse/symlink/replace/truncate behavior, capability/
scope/terminal response tampering and GUI page navigation/shared-job interactions.
Text-aligned diff, output search/index/merge and durable raw archives, larger mixed
policies/templates and remaining Phase 2–4 code and platform gates remain pending.
