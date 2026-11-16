# ADR-0028: Task result snapshots, export and bounded output comparison

- Status: Accepted
- Date: 2026-10-08

## Decision

IPC minor 27 adds TASK_RESULTS (bit 32), Results and Compare operations. Both also
require TASK_CONTROL. Results returns one durable revision with frozen host/step
names, states, timestamps, reasons, actual exit codes and derived counts. Commands,
variables, environment, credentials, auth references and output bodies are omitted.
Revision zero requests the latest readonly snapshot; a nonzero revision must match.
Ordinary Get/List remain available. Snapshot format cshell.task-results v1 is
bounded at 12 MiB and 1,000 targets × 32 steps, with structural and derived-count
validation. It reports incomplete runs honestly, including Unknown.

Application code owns pure counts, outcome grouping, filters and JSON/CSV export.
CSV writes one row per target/step and prefixes textual cells as text, escaping
controls and quotes. File output streams through a 64 MiB limit. Desktop and CLI
publish a synced temporary file without overwriting the destination; the selected
snapshot/revision is immutable during export. Export contains all snapshot targets,
independent of display filters. GUI workers own file I/O; no file path is sent to
the daemon. The daemon owns no client export destination.
The shared publisher is part of IPC's local runtime support and contains no task
business rules; formatting and resource limits stay in the application model.

Compare requires a terminal run with no active runner and an exact saved revision.
It compares two members of that frozen run, one common step and one stdout/stderr
stream, at the same byte offset. At most 64 KiB is read from each stream. Existing
empty files are distinguishable from missing/unavailable files. The document
reports sampled lengths, byte ranges, equality within the page and coverage of each
sampled stream, never full-output equality for a partial page or an atomic capture
of both files. Unknown remains an uncertain
execution result even when its saved partial output can be inspected. Raw byte
buffers clear on drop and Debug reports counts only. GUI comparison uses the
existing wgpu LogSurface, escaping bytes without interpreting terminal sequences.

Full-output hashing/diff, stdout/stderr chronological merging, indexed search,
output archives and multi-host SFTP remain subsequent P2 work. This iteration
writes implementation code only; compilation, tests and native acceptance are
deferred per the user's instruction. Prior quality evidence does not cover it.
