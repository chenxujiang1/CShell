# ADR-0024: Streamed SSH task execution and daemon control

- Status: Accepted
- Date: 2026-10-08

## Execution and output

The P2-050 production adapter uses task-owned SSH connections and non-PTY exec
channels. Credentials are loaded against the original frozen Profile; strict
known_hosts verification uses the resolved destination and each jump. No trust
is added by a task. Vault lock generation fences connection establishment.

Exec requests ask for acknowledgement, preserve all output (including data before
the acknowledgement), and send stdin EOF. Only an explicit failure before any
acceptance/output proves ExecRejected. Missing or conflicting status, signals,
transport loss and cancellation after dispatch remain uncertain. A status is
returned only after channel close and output sync. Channels and connections are
closed on cancellation; remote process termination is not guaranteed.

Raw stdout/stderr are spooled separately in protected, UUID-derived directories.
Each step creates new files without overwrite, accepts chunks up to 64 KiB, and
limits combined output to 1 GiB. Reads are offset-based and capped at 64 KiB.
No remote path or command enters a filename or diagnostic. Output failure stops
execution rather than truncating or claiming success. Partial files survive for
inspection; no completion is inferred from their existence. File sync is required
before acknowledging success; Windows power-loss directory durability remains
an acceptance item. Output is deliberately persisted ordinary task data.

## Daemon control

IPC minor 24 adds TASK_CONTROL (bit 29), envelope 47/48. Creation accepts a task
definition and host selection with both catalog revisions; the daemon freezes
the targets and persists the run without executing it. Starting requires explicit
confirmation and the saved run revision. Start is valid only for Queued or a
Paused recovered run; duplicate owners and terminal runs are not
replayed. Cancellation is bound to the immutable run ID, bypasses storage/control
waits, and acknowledges the in-memory request without claiming durable completion.
Gate approval
is bound to its persisted ID and revision. Mutations are never automatically
retried after lost replies. Lists are bounded and omit command/output bodies.

The daemon owns at most 64 runners sharing one resource budget. IPC disconnection
does not cancel them. Read operations recover orphaned persisted Running or
Cancelling records conservatively, without starting new connections or replaying
commands. Queued runs and approval gates require explicit Start after restart.
CLI uses authenticated discovery/IPC and performs no SSH or storage work.

The desktop result console, multi-host SFTP, comprehensive live server/platform
matrix and Phase 3/4 remain subsequent work; loopback protocol tests do not replace
external SSH or native GUI acceptance.
