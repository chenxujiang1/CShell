# ADR-0041: Portable stopped-command raw output archives

- Status: Accepted
- Date: 2026-10-10

## Archive scope and meaning

Extend P2-060 with one portable archive for one stopped command on one frozen host.
Capture reuses the exact ordinary run revision or mixed parent-target revision and
original child derivation from output analysis. It requires a single-target stdout
scope (the archive contains both streams), no active owner and actual terminal
native command/step receipts. File steps and unstarted suffixes are rejected.
Already completed mixed command prefixes can be captured before separately reviewed
later steps. No SSH connection, replay, profile/credential access or task mutation
is performed by archiving.

The archive retains raw stdout and stderr, plus `events.bin` bytes when available.
Both raw files must exist, including legitimate empty files; missing output is not
invented as an empty successful stream. Missing chronology is permitted for older
outputs and is represented by the absent Journal entry. Existing journal bytes are
copied as observed, including unsealed/truncated/partial records; this capture does
not claim that their chronology is valid or complete. Per-entry digests verify the
copied bytes. Original native statuses, especially Unknown, remain in the manifest.
Whole-file integrity does not prove remote success or complete output delivery.

## Container v1

`CSHOUT01` is an uncompressed, self-contained byte container:

1. Eight-byte magic and a little-endian u32 canonical compact manifest JSON length.
2. `cshell.output-archive` v1 manifest, at most 64 KiB.
3. Stdout, Stderr and optional Journal payloads in that fixed order, with declared
   lengths and SHA-256 digests; no user-controlled entry filenames or paths.
4. A 32-byte SHA-256 of all preceding container bytes.

The manifest includes a fresh archive UUID, original typed scope, frozen snapshot
digest/child binding/native states, capture time and entry metadata. It omits command
bodies, variables/environment, credential configuration, host display data and local/
remote paths. Payloads are original output and may contain sensitive data. The
artifact descriptor additionally carries total encoded bytes and SHA-256 of the
complete file including its footer. Hashes provide integrity, not encryption or a
signature/proof of origin.

Raw streams share the existing combined 1 GiB limit. Optional journal bytes are
bounded to 4,718,736; manifest/header/footer add at most 65,580. The application
formatter, inspector and transfer collector share these limits. Inspection enforces
canonical manifest encoding, entry order/length/digests, footer and exact EOF. It
returns metadata only and never extracts, interprets or executes payload content.

## Daemon capture and retention

Capture runs in the same global two-reader pool as analysis/search/merge. It pins
protected regular-file handles, computes entry digests, rewinds and rechecks each
digest while encoding directly into one private spool file, then syncs and rechecks
original handle/path metadata. Thus no whole stream is buffered and no extra raw
copy is needed. Progress separately reports source digest bytes and encoded archive
bytes. The ten-minute cooperative deadline and cancellation checks occur between
reads/writes and before completion; OS calls can delay observation. Slots remain
held until blocking workers exit.

Spools live under a private output-store archives directory with fresh temporary
directories and typed metadata. Successful files are reopened readonly; positional
reads avoid shared seek-position races. Job metadata retains at most 32 entries,
preserves running jobs and expires finished jobs after ten minutes. Completed spool
ownership is capped at four captures and 2 GiB; oldest-access expiration wins eviction.
Chunk reads refresh the retained capture's idle TTL. Evicted capture metadata may
still say Completed, but reads fail Missing and require an explicit recapture.

Restart loses cache hints and never recaptures or resumes automatically. Ordinary
expiry/eviction/cancel/error uses temporary-file ownership cleanup; hard crashes or
filesystem cleanup failures can leave private orphan spool directories. This iteration
does not silently delete unknown directories at startup. Orphan cleanup and broader
disk-retention policy remain a separate task. Cache size limits describe owned live
captures; in-flight workers/read references and cleanup remnants are additional.
Original output files are never moved, replaced or deleted.

## Protocol

IPC minor 37 / TASK_OUTPUT_ARCHIVE bit 41 adds OutputArchive (50), Get (51), Cancel
(52) and Read (53), requiring TASK_CONTROL and, for mixed clients, MIXED_TASK_CONTROL.
Launch carries the existing scope in definition_json and the matching top-level
run UUID. Get/Cancel use the job UUID in result_id. Read uses the separate archive
UUID plus offset/limit, at most 1 MiB per chunk. Canonical requests reject unrelated
fields. Get/Cancel/Read use a direct path rather than waiting for task storage work.
No SQLite schema, dependency version or new protobuf field is required.

`cshell.output-archive-job` v1 has Running/Completed/Failed/Cancelled, monotonic
source/encoded-byte counters, original binding and only a Completed artifact or fixed
Failed category. `cshell.output-archive-chunk` v1 binds run/archive identity, complete
artifact digest, total length, exact offset/next/body size and chunk digest. Metadata
is bounded to 64 KiB; only Read permits a binary output body and output_length must
match it. Reads also retain a reader slot until native I/O ends.

## Client download and publication

The shared application collector requires exact ordered chunk sizes/offsets and
digests, limits aggregate bytes and verifies the full artifact digest without
buffering the payload. GUI/CLI stream authenticated chunks into a private staged
client file, sync, rewind and inspect the entire container against the captured
descriptor, then publish to a new absolute destination without replacement. Paths
never go to daemon. Staging rejects an already present destination early and still
uses atomic publication without overwriting to handle a later race. Publication
or final sync/cleanup uncertainty instructs the user to inspect the destination
before a new explicit attempt; no automatic retry occurs.

The new ExportStaging helper owns its file before its temporary directory so handle
closure precedes cleanup, tightens private staging permissions, and shares the
existing sync/persist-without-replacement semantics. It leaves the destination
absent until the final publication step. ExportTransferControl records local progress
and atomically arbitrates CancelRequested versus Publishing. A cancellation that
wins before that decision discards staging; cancellation after Publishing cannot
claim to prevent the file. Download plus final verification has a thirty-minute
cooperative deadline; each IPC request retains the existing ten-second deadline.

Desktop ordinary/mixed panels share an independent archive connection retaining
one job and local transfer. It provides capture/status/cancel, source and encoded
progress, explicit original scope/native state, artifact/entry digests, and separate
confirmation for downloading raw content. Confirmation resets on scope, document,
path change and deactivation; export binds the exact cached job object. The captured
destination is displayed separately from an editable path. Local download cancellation
is independent of daemon capture cancellation. Neither grants execution authority.

CLI adds output-archive <single-target-stdout-analysis-request.json>, get/cancel
<job.json>, export <completed-job.json> <absolute-new-file>, and inspect <archive.cshout>.
Capture/job replies contain metadata only. Export explicitly downloads that completed
artifact and saves raw content; offline inspect requires no daemon and prints the
validated descriptor. Commands use existing authentication/capability/reply identity
checks and do not launch or retry another capture implicitly.

## Status and deferred verification

Code and source formatting only under the user's instruction. Compilation/tests/
audit/MSRV, migrations, native GUI/CLI/IPC, actual capture/hash/download/inspection/
publication and daemon replacement are deferred. Original daemon/live DB/package
lock remain unchanged; old proof does not cover this WIP and no P2 gate closes.

Consolidated verification must cover empty/missing/binary/legacy/partial-journal
inputs, two-pass mutation and fixed-handle races, manifest/header/entry/footer/EOF
tampering, length/byte limits and exact chunk progression, ordinary/mixed version/
child/terminal/owner gates, capture/download cancellation and publication arbitration,
progress/slot retention, cache byte/count/TTL/eviction/restart, private-file/platform
permissions and cleanup failures, destination races/uncertainty, offline inspection
and GUI confirmation/target-path/shared-connection behavior. Multi-command/host
archive bundles, extraction/import UI, orphan/retention policy, aggregate indexes/
search/merge, text diff and remaining mixed policy/template/Phase 2–4 gates continue.
