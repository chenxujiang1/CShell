# ADR-0040: Per-command stdout/stderr recording journal and merged pages

- Status: Accepted
- Date: 2026-10-10

## Chronology and writer boundary

Extend P2-060 with stdout/stderr merging for one command on one frozen host. New
task output writers create a private `events.bin` beside the existing raw stream
files. Each nonempty written chunk records a strictly increasing sequence, local
recording time, stream label, original stream offset/length and SHA-256 of its bytes.
Time is captured before that chunk's raw write using a wall anchor plus monotonic
elapsed time. Sequence is authoritative within this writer; neither timestamp nor
sequence claims remote process-write order or a total order across hosts/commands.

Existing command execution consumes events sequentially, applying disk backpressure;
mixed commands reuse the same child writer. The raw write completes before its
metadata record is appended. Journal I/O failures follow the existing OutputFailure
path; no execution status is manufactured from a journal. The writer refuses new
writes after its journal has been sealed.

Journal metadata is limited to 65,536 events. After that point raw writes continue
under their existing combined 1 GiB limit and the writer records that chronology
is truncated. Successful finish first flushes/syncs both raw files, then appends and
syncs a footer with event count, raw lengths, truncation and a checksum of the header/
event prefix. Seal means local file/index finish, not remote execution success or
complete output delivery. A crash/cancel/read/storage failure may leave an unsealed
prefix; Unknown remains Unknown even if some local files happen to be sealed.

## Journal v1 and reading

`CSHTEV01` uses 72-byte fixed blocks. The header binds child run UUID, Host UUID,
actual child step and initial recording time, with zero reserved fields. Event
types 1/2 distinguish stdout/stderr and contain sequence/time/offset/length/content
SHA-256. Footer type 3 contains truncation, event count, final time/raw lengths and
prefix SHA-256. Maximum journal size is (65,536 + 2) * 72 = 4,718,736 bytes.

The reader reuses protected typed paths and pinned non-link/non-reparse regular
handles from output analysis. It scans the bounded journal with a 64 KiB buffer,
validates identity/version/reserved fields, every sequence/time/stream-offset range,
footer placement/lengths/checksum, and computes the whole journal SHA-256. A final
partial block is explicitly reported as an unsealed tail and contributes to that
digest; only complete validated events are available. Changed handles/path metadata
or selected content witnesses fail rather than producing a successful partial page.

A page contains at most 64 consecutive events and 64 KiB combined raw bytes. Byte
capacity can end a page before its event limit; later smaller events are not skipped.
Each selected event's bytes are read from its original stream, checked against its
recorded digest, and carry a page-body offset. Cursor is the last returned event
sequence and continuation binds the prior whole-journal digest. The reader reports
actual stdout/stderr lengths, indexed prefix lengths, total event count, seal,
truncation and partial tail separately. Even an empty/unsealed journal does not turn
unindexed raw data into invented chronology.

Old output without `events.bin` returns Missing with a specific unavailable-chronology
message. It is neither rewritten nor assigned guessed timestamps. Existing single
stream reading, digest/comparison and literal search remain available. This format
is a per-command recording index; durable portable raw archives and cross-host
aggregation remain separate work.

## IPC and clients

Minor 36 / TASK_OUTPUT_MERGE bit 40 adds OutputMerge (49), requiring TASK_CONTROL
and, for mixed clients, MIXED_TASK_CONTROL. Definition_json contains
`cshell.output-merge-request` v1 with an existing exact single-target output scope,
event cursor, limit and optional journal digest. The scope's stderr flag must be
false because this operation reads both streams; right-side targets are rejected.
The daemon reuses stopped-command version/owner/native-receipt and child derivation
checks. Files and unstarted suffixes cannot use merging.

Reply metadata is `cshell.output-merge` v1, with unchanged frozen bindings and a
separate bounded binary TaskResponse.output body. Revision is zero and output_length
equals body length. Client decoders validate shape, scope, cursor, all count/coverage/
offset/hash constraints and each chunk's content before presenting a page. Pages
use the same global two-reader semaphore as full analysis/search. The bounded scan
has a cooperative five-second check between groups of journal records; OS calls can
delay observation and retain the slot. IPC deadlines never spawn a replacement read.

Desktop ordinary/mixed windows share an independent background merged-page connection.
Each panel owns its expected request, cached metadata and LogSurface/reflow state;
another window's cache update does not remove the metadata for its displayed page.
It offers first/next pages, seal/coverage/capacity information and explicit return
to a single stream or an event's original byte page. Scope changes invalidate its
merge selection; output frame/scroll route to the merged LogSurface while selected.
Byte formatting occurs in the worker, includes sequence/time/stream/offset labels,
and escapes controls/non-ASCII/backslashes. No whole raw stream is copied into GUI
memory, no launch is automatic, and reads are sent once with bounded deadlines.

CLI adds output-merge-request <single-target-stdout-analysis-request.json>,
output-merge <request.json> and output-merge-next-request <page.json>. Helpers only
print typed JSON for explicit saving. Page JSON contains metadata and a byte array,
so it includes raw output and may contain sensitive data; next-request validates
those bytes and prints the digest-bound continuation without reading automatically.
Analysis/search/merge share capability/authentication/identity/shape checks. CLI adds
its existing workspace serde dependency for the saved-page wrapper; Cargo.lock only
adds that edge, with no package/version update. No SQLite schema change is needed.

## Implementation and deferred verification

Code and source formatting only, per the user. No compilation/tests/audit/MSRV,
migrations, native GUI/CLI/IPC, daemon replacement, real task/output I/O or publication
runs this iteration. Original daemon/live DB/package-lock.json stay untouched; no
P2 gate closes. New journals are created only when this implementation eventually
runs new commands, not by rewriting the current live output directory.

Consolidated verification must cover empty/two-stream/binary/interleaved chunks,
same-millisecond sequence, exact metadata capacity and raw continuation, seal/sync/
partial-footer/crash/loss checkpoints, checksum/identity/reserved/time/offset tampering,
missing legacy journals, byte-capacity paging/no skipped events/digest continuation,
ordinary/mixed scope and child rejection, reader-slot/deadline behavior, native path/
file races, per-window cache/caption/frame/scroll/reflow changes and CLI raw-page
roundtrips. Portable durable raw archives, cross-host merge/index/search aggregation,
text diff and remaining mixed policies/templates/Phase 2–4 gates continue separately.
