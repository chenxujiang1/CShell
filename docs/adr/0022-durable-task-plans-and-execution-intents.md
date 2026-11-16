# ADR-0022: Durable task plans and execution intents

- Status: Accepted
- Date: 2026-10-01

## Scope

This P2-050 foundation implements immutable command plans, deterministic batches, typed execution transitions and a durable repository. It does not yet start SSH connections, dispatch workers, enforce shared global/endpoint/jump budgets or wire task control into the daemon, IPC, desktop or CLI. Those integrations remain part of P2-050 and the original Phase 2–4 plan.

This describes the initial foundation commit. The subsequent shared-budget asynchronous scheduler is recorded in [ADR-0023](0023-shared-task-budgets-and-durable-dispatch.md); its production SSH and daemon/client adapters remain in progress.

## Immutable plan and shell compiler

A TaskSnapshot has a new TaskRunId, operator, creation time, task definition, frozen Host/Profile configuration, compiler version, compiled commands and canonical SHA-256 fingerprint. Its validated private data is shared through Arc; cloning a transition does not copy the entire configuration snapshot. Loading verifies the version, bounded document and exact compiler output. Subsequent library changes cannot replace its destinations, credentials or command semantics.

Shell selection is explicit: Posix, PowerShell, Cmd or a trusted Custom launcher with one {script} slot and an explicit dialect. Host platform hints never select the shell. Static scripts are trusted source and receive no application variable substitution. Dynamic values are accepted only as structured invocation arguments and are quoted for the selected dialect. Missing variables and unsupported values fail before a run is created.

Posix uses quoted /bin/sh -c source. PowerShell uses UTF-16LE EncodedCommand and propagates native exit codes; its wrapper reports terminating errors on stderr. Cmd disables AutoRun and delayed expansion, rejects multiline source and restricts dynamic arguments/environment values to a conservative ASCII alphabet. When Cmd needs environment overrides, an encoded PowerShell launcher sets the child process environment before Cmd parses %NAME%, concurrently copies raw stdout/stderr and preserves the Cmd exit code. This form requires powershell.exe on the target. Custom launchers are trusted configuration, not a sandbox or a guarantee of arbitrary shell compatibility.

Definitions permit 32 steps, 64 variables and 64 environment overrides, 256 arguments per invocation, bounded strings, a 1 MiB definition and a 12 MiB snapshot. Each compiled command is limited to 512 KiB and a plan to 2 MiB compiled source. These are application resource bounds, not a promise that a remote shell accepts every command of that size. Persisted scripts, variables and environment are ordinary task configuration; Debug and execution events omit their bodies. No claim is made that ordinary configuration is secret storage.

## Transitions and result certainty

TaskRunRecord stores a revisioned run, ordered target rows and per-step status/timestamps/actual exit code. Only validated actions construct opaque patches bound to the exact snapshot fingerprint and previous metadata. The caller prepares a patch, commits it, then applies the acknowledged revision. An uncertain repository result is not permission to replay an execution or a mutation.

The future executor must persist StepIntent before calling SSH exec. Once intent exists, cancellation, timeout, lost transport, output failure or daemon restart cannot prove that execution did not occur: the active step and target become Unknown. A definitive exec rejection is Failed. A known exit status remains authoritative even if cancellation has been requested; failed steps cancel the remaining steps. Cancelling before dispatch marks queued targets Cancelled. Aggregate results retain prior failures and Unknown rather than hiding them behind a later cancellation.

Connect checks the current frozen batch, per-run concurrency and failure threshold. Batch advancement requires terminal targets. A failed canary stops queued targets. Optional canary/between-batch approval creates a fresh one-use gate ID bound to the next batch; restore checks the policy, prior batch completion and canary success. Recovery preserves queued work and paused gates, changes Connecting/Running targets to Unknown and never re-executes them. Shared resource budgets, actual delays/deadline enforcement and cancellation of transport channels still require the scheduler integration.

Explicit retry creates a new run and a new frozen subset from the original configuration. It cannot include succeeded or nonterminal targets. Retrying Unknown requires recorded confirmation and an idempotence declaration on every step; this never grants automatic retry. The repository validates the retry parent, selected terminal states, original command definition, destinations, connection closure and catalog revisions before insertion. Original results remain unchanged.

## SQLite durability

Schema 12 adds task_runs, task_targets and task_run_events. Supported older schemas are backed up before their complete migration is committed in one transaction; failure preserves the old version and data. Initial creation must equal the canonical revision-1 record. The immutable snapshot is stored once; later commits update metadata, changed target rows and metadata-only events together. CAS revision, fingerprint and exact previous rows reject stale or substituted patches. A failure after target/metadata writes but before event insertion rolls back the whole transaction.

Loads cap stored snapshot, metadata and target blobs in SQL before allocation, validate column/document agreement and check contiguous event revisions. Bounded summary pagination returns target counts without loading script bodies. Backup/reopen/restore uses the existing repository path. The fingerprint binds normal application transitions to a snapshot; it is not an authenticated journal against an actor able to rewrite the database.

## Verification boundary

Regular regressions cover 1,000 model targets and stable canary batches, snapshot/compiler corruption, limits and Debug privacy, explicit retry and Unknown confirmation, recovery, one-use gates, concurrency, cancellation/failure aggregation, clock and stale acknowledgement rejection, SQLite CAS/rollback/reopen/backup/oversized blobs, retry substitution and v11 migration rollback. Windows runs native hidden powershell.exe/Cmd processes to check literal arguments, UTF-16 Unicode source, process environment, separate output streams and actual exit codes. A Unix-specific /bin/sh argument oracle is included for its platform runs.

These tests do not establish live SSH execution, shared scheduler fairness, output spooling, current-platform task IPC/UI, 1,000 live connections, native GUI/IME or the four-platform acceptance matrix. P2-050 remains in progress; result console, multi-host SFTP, CLI and subsequent phases retain their original scope.
