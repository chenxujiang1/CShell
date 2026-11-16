# ADR-0039: Bounded literal search of stored task output

- Status: Accepted
- Date: 2026-10-10

## Scope and byte semantics

Add P2-060 readonly search for one stopped command stdout/stderr stream in an
ordinary or mixed run. The request reuses the exact OutputAnalysisRequest scope:
ordinary run revision or mixed parent-target revision, frozen Host, step and stream.
Right-side references are rejected. The shared daemon binding routine derives the
original command child UUID/step and preserves the existing terminal-native-receipt,
owner, file-step and unstarted-suffix restrictions. Unknown is still an observation
of incomplete knowledge and no match/no-match result changes execution status.

`cshell.output-search-request` v1 carries a nonempty literal byte pattern of at most
1,024 bytes, a match-start offset, limit 1–256 and optional expected whole-stream
SHA-256. UI and CLI helpers accept UTF-8 literal text or hex byte pairs with optional
ASCII spaces. Matching is case-sensitive, does not decode output or normalize line
endings/ANSI sequences, and permits overlapping matches. Offsets are original byte
positions. Regex, case folding, multi-host aggregation and persistent indexes are
separate remaining features.

The worker uses prefix failure links to preserve linear matching across 64 KiB
buffers and repetitive data. It returns at most the requested match count and
looks for one additional match to establish whether another batch exists. A next
cursor is the last returned match start plus one, so overlapping matches remain
available without repeating a returned offset. Without an additional match, next
is absent and matching is complete from the requested start through EOF.

## Stable continuation and resource bounds

Every successful batch hashes the entire local stored stream, including bytes before
the requested start and after the retained match limit. The result carries full
length/SHA-256, match offsets, matching-scan boundary and optional next cursor. The
hashing progress and matching-scan boundary have different meanings when a batch is
full. Continuation uses the same kind/run/Host/revision/step/stream/pattern digest/
length/limit and the prior result's full stream hash. The server recomputes that hash
and fails Changed if the stream differs. A request already pinned to another hash
cannot use a mismatching completed document to generate a continuation.

Search reuses fixed read handles, private-path/regular-file/no-link/reparse checks,
Unix identity/change metadata, Windows shared-read-only behavior and final handle/
path checks from ADR-0038. Missing is not empty, and failed/cancelled/changed searches
return no partial successful result. Full means the local file, not proof that all
remote output arrived or that the command succeeded. Each batch rereads the whole
file; this is an explicit bounded scan rather than an indexed search.

Search and full digest/comparison jobs share the same global two-worker semaphore.
Each search uses one 64 KiB buffer, a pattern/failure table of at most 1,024 entries
and at most 256 retained offsets. Existing 1 GiB stream limits apply. Cancellation
and the five-minute cooperative deadline are checked before opening, between reads
and before completion; OS calls can delay observation. The slot remains held until
the blocking worker exits. No replacement scan or continuation starts automatically.

## Protocol and ephemeral jobs

IPC minor 35 / TASK_OUTPUT_SEARCH bit 39 adds OutputSearch (46), OutputSearchGet (47)
and OutputSearchCancel (48), requiring TASK_CONTROL and, for mixed clients,
MIXED_TASK_CONTROL. Launch uses definition_json for the typed request and the same
top-level run ID. Query/cancel use run ID and an opaque job ID in result_id. Canonical
shapes reject unrelated fields; no wire field, dependency version or schema change
is needed. Existing full analysis and mixed result documents keep their shapes.

`cshell.output-search` v1 job documents contain the scope, pattern byte length and
SHA-256, start/limit/expected stream hash, frozen binding, state/progress, optional
completed result or fixed failure category. Query bytes, matched output bodies,
paths and command/authentication data are absent. Pattern digests are observations,
not encryption of a low-entropy query. Raw query bytes exist in launch requests;
the application request zeroizes its owned byte buffer on drop and request debug
output omits it. The daemon caches only the descriptor and result metadata.

Search retains at most 32 jobs, preserving active entries; finished jobs expire
after ten minutes or are evicted oldest-first. Expired entries are removed on cache
access. Restart drops jobs and never resumes a query. Documents are bounded to
64 KiB and use Running/Completed/Failed/Cancelled with strict result/error presence,
byte/count/offset/cursor/hash checks. Metadata get/cancel bypass task storage waits;
cancel only reduces local scan authority.

## Desktop and CLI

Ordinary/mixed windows share one independent background search connection retaining
one current job document. Each panel has its own UTF-8/hex query draft, cleared on
deactivation; query editing, scope changes and original binding checks prevent old
results being used for the current input. Eligible stopped output views provide
launch, readonly progress polling, explicit status/cancel, and 128-match batches.
Continuation is explicit and reuses the current query only when its descriptor
matches the completed job; a changed parent revision requires a new query. Matches
open the existing 64 KiB byte page and LogSurface rather than copying entire streams
into the GUI. The shared connection checks exact descriptor/job/binding identity,
nondecreasing progress and stable terminal documents; missing jobs clear stale hints.

CLI adds output-search-request <single-target-analysis-request.json> <utf8|hex>
<pattern>, output-search <request.json>, output-search-get/cancel <job.json> and
output-search-next-request <request.json> <completed-job.json>. Request helpers print
JSON for explicit saving; request files contain query bytes, job replies do not.
Next-request prints a validated digest-bound request and does not launch it. Search
and full analysis share an authenticated transport with capability/reply-shape/run
identity checks and deadlines. Every launch/query/cancel is sent once.

## Status and deferred work

Implementation and source formatting only under the user's deferred-verification
instruction. No compilation/tests/audit/MSRV, migration, native GUI/CLI/IPC, daemon
replacement, actual output scan/hash or SSH/SFTP/file publication runs this iteration.
Existing daemon/live DB/package-lock.json remain unchanged; no overall gate closes.

Consolidated verification must cover empty/missing/binary/UTF-8/hex patterns, malformed
and oversized input, repetitive/overlapping/chunk-boundary/EOF matches, exact-limit
and extra-match pagination, start/EOF/offset bounds, full digest on every batch,
changed stream and expected-hash rejection, ordinary/mixed scope and original child
binding, owner/terminal/file/suffix gates, cancellation/deadline/shared slots, TTL/
eviction/restart, query omission/debug handling, native filesystem races and GUI
draft/result/navigation/shared-connection behavior. Text diff, persistent indexes,
multi-target search aggregation, timed stream merge, durable raw archives, mixed
policies/templates and the remaining Phase 2–4 code/platform gates continue.
