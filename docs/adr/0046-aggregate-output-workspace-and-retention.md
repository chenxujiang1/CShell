# ADR-0046: Aggregate output workspace, bundles and retention

- Status: Accepted
- Date: 2026-10-11

## Version-bound observations

P2-060/P2-080 add OutputWorkspaceRequest/Document/Page and a shared daemon job
pool for Index, Search, TextDiff, Bundle, RetentionPreview and RetentionApply.
A request binds up to 4,096 unique Host/step/stream scopes in one ordinary or mixed
run. Ordinary scopes bind the stopped run revision; mixed scopes bind stopped
command receipts and exact parent target revisions. Bind all scopes from one
repository observation and preserve snapshot hashes, native child IDs/states and
actual output bytes. No operation grants execution/retry/repair authority.

Metadata requests/replies are bounded to 8 MiB under the existing 16 MiB envelope
limit. Work shares the existing two readonly output slots, has a cooperative
ten-minute deadline and cancellation, and retains at most eight result sets for
ten minutes. Each encoded result is bounded to 64 MiB. Result pages contain up to
64 rows, capped by metadata bytes, and bind job ID/result digest and exact offset.
Job metadata contains query SHA-256, never query body or output text. Search requests
contain the explicitly entered query and are Debug-redacted. Text rows are returned
only through result pages and are redacted in Debug.

## Persistent indexes and aggregation

Every selected stream is pinned, fully hashed and checked after reading. The line
index stores scope/binding, full stream digest, total line count and up to 131,072
line offsets. It is private canonical JSON addressed by its own SHA-256. Index
publication/read/eviction are serialized within the output store. Existing content
must match, and cache limits are 1,024 indexes / 256 MiB with oldest eviction.
Indexes survive restart and can be read independently of an expired job using
OutputIndexRead with the original scope/index entry. The source revision/binding
and index content digest are checked; missing/evicted indexes are not fabricated.
Offsets are captured observations, not a guarantee that retained raw files still exist.

Index results group identical complete stream length/SHA pairs across hosts/steps.
Literal byte search uses a bounded overlapping KMP scanner, records source, byte
offset and line, scans/hashes complete streams, and returns up to 8,192 matches.
Truncation is explicit; a capped result does not claim all matches.

UTF-8 text diff accepts exactly two streams, at most 8 MiB / 20,000 lines each and
1 MiB per line. Line endings and final newline remain significant. Unique common
lines provide noncrossing patience/LIS anchors; small gaps use bounded LCS, large
unanchored gaps preserve common boundaries and emit deletions/insertions. The result
is deterministic and valid alignment; it does not promise a globally minimal edit
sequence. Binary/oversized inputs fail explicitly; original byte comparison remains
available. Rows preserve original line numbers and byte positions.

## Multi-output bundle and client extraction

Bundle captures each selected command through the existing two-pass CSHOUT01
capture, including stdout/stderr and available journal. CSBND001 contains a canonical
manifest of independently verifiable member artifacts, exact member bytes and a
container footer/full digest. Total bundle size is bounded to 2 GiB; all nested
checksums, metadata, lengths and EOF are validated. Captures preserve Unknown and
original state without interpreting file bytes as successful remote execution.

One bundle builder runs at a time, with one retained bundle. Working member copies
and the finished bundle can occupy up to 4 GiB; a concurrently held older read
capture can temporarily add up to 2 GiB. The existing single-archive cache has its
separate bounded budget. Downloads use 1 MiB chunks, exact frozen job metadata,
private client staging and full container verification before no-overwrite file
publication. Destination paths do not cross IPC.

Single archives and bundles can be inspected and extracted offline through shared
application parsers. Extraction first validates the whole container, then rewinds
the same handle and revalidates content while writing private staging. Names are
generated solely from typed run/Host/step/stream identifiers; archive content never
supplies paths, permissions, symlinks or executables. A manifest is included.
ExportDirectoryStaging exclusively reserves a new absolute destination directory
and publishes only flat generated regular files without overwriting. Cancellation
and a 30-minute client deadline apply before the publication decision. Directory
publication can be partially visible on failure and is explicitly reported; no
automatic replay or deletion of published paths occurs.

Generated archive/bundle directories now hold a locked lease.json with typed run ID
and creation time for their full lifetime. Cleanup checks only recognized private
spool namespaces, obtains the exclusive inactive lease, requires age at least 24
hours and permits only the lease and known archive/bundle file. It removes individual
files and an empty directory, never recursively traversing arbitrary paths. Live
captures, unrecognized/legacy spools, symlink/reparse entries and unexpected contents
are retained. Cleanup runs at startup and capture/workspace entry. Clean shutdown
still uses the owning TempDir; hard-exit leftovers can be reclaimed later.

## Explicit raw-output retention

RetentionPreview digests selected stopped commands' stdout/stderr and optional
journal and creates a ten-minute OutputRetentionPlan with exact scopes, bindings
and content. Apply requires that plan plus explicit confirmed=true, rebinds current
metadata, and hashes all sources before claiming an exclusive plan directory.
The exclusive claim survives restart and prevents reusing the same plan.

Persist OutputRetentionReceipt before any mutation, with one state per source.
For each source, check the pinned originals, release handles for Windows rename,
quarantine its typed step directory, persist Quarantined, and rehash the private
moved regular files before deletion. Only stdout.bin/stderr.bin/events.bin may be
removed. Unexpected files, changed content, cancellation or storage failure stop
the operation with already committed steps preserved. Pending/Quarantined receipts
can represent interruption or partial native effects; they do not prove no deletion.
No automatic restore, replay or fabricated empty output is performed.

Receipts remain readable by plan ID after job expiry/restart. Run metadata, command
intents, actual native receipts, backup files and persistent index metadata are
retained. GUI disables further deletion submissions after an uncertain/failed/
cancelled apply and offers durable receipt lookup; complete Deleted receipts clear
that uncertainty. A new reviewed preview is required for further applicable outputs.

## Protocol and adapters

Minor 41 / OUTPUT_WORKSPACE bit 45 introduces operations 57–63: launch, get,
cancel, result page, bundle read, retention receipt and persistent index read.
Existing TaskRequest/Response fields are reused with strict canonical shapes.
Minor 42 additionally carries ADR-0047's independent file-publication capability.
No SQLite migration or new dependency version is needed.

The shared desktop connection/panel integrates ordinary and mixed task consoles,
cross-page output selection, action/query confirmation, job refresh/cancel, digest
groups, diff rows, result/index pages, bundle downloads and offline inspect/extract.
Mixed selections retain exact versions across pages; changed scopes invalidate
selection/confirmations. New job/retention plan identities clear approvals. Missing
jobs clear stale running views while preserving uncertain deletion plans.

CLI adds workspace scope/request generators, launch/get/cancel/page/export,
index-read, retention-apply/receipt and offline container-inspect/extract. Scope
generation uses existing decoded status documents and emits only eligible stopped
outputs; oversized sets are rejected instead of silently truncated. All execution
and retention mutations remain daemon-owned; client adapters share parsers and
publication helpers. Requests are sent once; unknown replies are not retried.

## Verification deferred

Source implementation and cargo fmt only. The user deferred compilation, tests,
audit/MSRV, schema/native GUI and actual SSH/SFTP/filesystem acceptance. The unified
matrix must cover scope binding/epochs, full-file changes/missing/binary/UTF-8/endings,
index hash/reopen/eviction/quotas, KMP overlap/caps, alignment work bounds, all metadata/
page/member/container/EOF tampering, cancellation/deadlines/slot release, client staged
publication and partial directories, live leases/legacy/hard-exit cleanup, retention
preflight/quarantine/restart/partial commit/receipt integrity and no implicit execution.
