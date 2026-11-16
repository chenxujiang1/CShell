# ADR-0023: Shared task budgets and durable dispatch

- Status: Accepted
- Date: 2026-10-04

## Scope and ownership

P2-050 now has a reusable asynchronous TaskScheduler over the durable TaskRepository and TaskExecutor/TaskConnection ports. It runs an already persisted TaskRun and returns its final record after worker cleanup. The production SSH adapter, bounded stdout/stderr spooling, daemon service/IPC, desktop task controls and CLI remain subsequent integrations. Model executors and SQLite integration tests do not represent live SSH acceptance.

All runners in one service must share one TaskResourceBudget. It atomically reserves a global target slot, a per-run slot, a destination slot and every intermediate SSH jump/proxy endpoint. Defaults are 50 globally, one per destination and ten per jump; run concurrency comes from the frozen policy and is limited to 50. Configured global capacity is bounded to 1,000. Reservations either acquire every resource or acquire none, so workers cannot deadlock while holding a partial route. Non-cloneable leases remain held across connection, every step and bounded cleanup, then release all counters and remove zero-count keys. Endpoint configuration is omitted from Debug.

Resource keys use actual frozen address/port pairs rather than Host or Profile UUIDs. IP literals are canonicalized; DNS spelling ignores ASCII case and trailing dots. Profile aliases for the same jump share a limit. Different DNS names are not resolved to prove physical equivalence. The jump budget counts concurrent targets using each distinct intermediate endpoint. Per-destination and intermediate counts are separate resource roles. Acquisition is opportunistic with bounded polling; no strict FIFO fairness is claimed.

Frozen targets must be in strictly increasing Host ID order. Task target lookup uses binary search, matching SQLite's ordered rows; malformed reordered snapshot documents are rejected before insertion or execution. This removes repeated linear scans from validation during large runs.

One run owner is admitted per TaskRunId in a shared budget, with at most 64 concurrent owners. A duplicate runner cannot mark a live owner's targets Unknown by invoking startup recovery. This is in-process ownership, not a database lease across independent services/processes; the future daemon service must provide the single owner registry and shared budget.

## Dispatch and cancellation

The actor owns the run record and serializes CAS commits. Connect is persisted before opening a target connection. A worker then requests a step grant; StepIntent must be acknowledged before the actor supplies the compiled command. Each known exit code is published and durably recorded before a subsequent step can be granted. The executor processes output before reporting an exit. Output storage and SSH verification remain obligations of the production adapter.

Workers receive both the direct user cancellation watch and their target-specific timeout/stop watch. A request can therefore stop a pending connect/exec future while the actor waits on storage. The actor checks cancellation and deadlines again after each commit and before granting work. Pending targets are not dispatched after cancellation. Already granted work can race a cancellation request; remote termination is never assumed. Known exit codes remain authoritative, including a code queued concurrently with cancellation. A late known step result is retained; remaining steps are stopped if its deadline has expired. An interrupted active intent becomes Unknown, while a target stopped between proven completed steps can be TimedOut or Cancelled without inventing uncertainty for the completed step.

Approval requests use a 16-entry queue and acknowledge durable gate approval. Known results and completed workers are processed ahead of approval traffic. Gate IDs remain exact and single-use. Closing a UI control handle does not cancel background work. Close is limited to three seconds and must close task-owned channels; adapter drop/future cancellation must release owned resources without closing a shared SSH transport. Dropping the runner cancels/aborts its workers; their persisted intents are reconciled on the next explicit startup.

## Time and recovery

SystemTaskClock anchors wall time once and adds monotonic elapsed time, preserving persisted epoch timestamps during a live run without following backward wall-clock adjustments. A restart still rejects a clock earlier than the durable record. Storage calls have five-second deadlines; errors, timeouts, conflicts, worker failures or mismatched acknowledgements stop dispatch, cancel remaining workers and return a typed error. No mutation or command is replayed automatically.

Connection and step futures have independent timeouts. The actor also checks persisted start times, total duration, failure limits and queue age with a 20 ms poll interval. The total deadline includes batch delays and manual gates. Queue age applies to pending targets in the eligible batch, starting when that batch can dispatch; future batches and paused gates do not consume their queue allowance. The first batch starts at submission time.

TaskRunMeta now persists batch_not_before_ms without changing the SQLite schema or immutable snapshot format. AdvanceBatch records the delay once. Approval preserves that deadline or moves eligibility to its approval time. Recovery preserves the stored deadline. Older foundation metadata lacking this field uses a conservative delay after its latest update; approving such an old gate waits a full configured delay from approval. Startup changes Connecting/Running targets to Unknown and resumes only Pending work. Paused gates and existing terminal results are retained.

## Verification boundary

Regular tests cover shared destination/run/jump limits, atomic reservation failure and lease release, a 1,000-target model run with at most 50 live model connections, pre-dispatch cancellation, cancellation during connect/intent commits, lost intent acknowledgement, distinct connect/step/queue/total timeouts, gate/delay control, known exit/cancel/deadline races, failure thresholds and duplicate-run rejection. Real SQLite tests verify the intent barrier before every model exec, a reopened gate/deadline, complete multi-step execution, transactional event failure and recovery without execution replay.

These are production scheduler and storage paths with controlled executor implementations. Live non-PTY SSH channels, output limits/durability, host-key/authentication acceptance, daemon/client integration, native UI and the current-source platform/P2-099 matrix remain open. Original Phase 2–4 scope is unchanged and P2-050 is not marked complete.
