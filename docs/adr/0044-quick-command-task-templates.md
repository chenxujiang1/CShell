# ADR-0044: Quick command task and script templates

- Status: Accepted
- Date: 2026-10-10

## Saved actions

Extend P2-030 QuickCommandAction with RunTask and RunScript. Both store a complete
TaskDefinition and HostSelection, without live sessions, credentials, target
snapshots, execution receipts or approval grants. RunTask supports the existing
ordered task steps. RunScript requires exactly one TaskCommand::Script step and
uses the remote task engine. It does not introduce the later QuickJS automation
runtime or its permission model.

Definitions use the existing task validator and shell compiler, retaining explicit
Shell/custom launcher, Script/Invocation commands, literal/variable arguments,
variables, environment, success codes, idempotence and execution policy. Script
text is not interpolated. Selection uses the existing bounded selector/group/
explicit codec; an empty explicit set is rejected. Group/profile existence and
current host membership are resolved when saving the new task plan.

Task definition JSON is limited to 256 KiB, selection to its existing 64 KiB limit,
and the entire catalog remains limited to 1 MiB, 256 commands and 64 folders.
Action Debug continues to redact the entire payload. Templates are ordinary
configuration and require the existing explicit persistence acknowledgment.

## Catalog and protocol compatibility

Existing text-only catalogs keep format cshell.quick-commands version 1. Adding
the first task/script action promotes the catalog to version 2. Both versions are
readable by the new decoder, but version 1 must not contain task actions. Edits
keep version 2 after the last task action is removed; no implicit downgrade occurs.
The existing SQLite schema 10 JSON row and revision CAS are reused, with no new
tables, migration, dependency or repository API.

IPC minor 40 adds QUICK_TASK_TEMPLATES bit 44 alongside QUICK_COMMAND_CONTROL.
Envelope/request/response fields are unchanged. The desktop requests both bits,
requires the new bit before sending any version 2 save, and otherwise retains
the original revision/content acknowledgment checks and uncertain-write behavior.
Daemon rejects version 2 load/save without the negotiated bit. Under the existing
catalog mutation lock it also rejects replacing a stored version 2 catalog with
version 1, preventing an older client from dropping actions it cannot understand.
The storage repository still performs the authoritative transactional revision CAS.

## Desktop editing and use

The quick-command editor offers SendText, OpenCompose, RunTask and RunScript.
Task actions expose bounded complete-definition and selection JSON editors, with
a starter document and validation before opening or saving. Existing folder/tag/
system organization and save/delete confirmations apply to all four actions.

Using a task action validates it and creates a request bound to the owning
WorkspaceWindowId. Only that window consumes the request. Source/focus/security
invalidation clears pending requests; closing Compose clears its transient JSON.
Task use does not pass through terminal input review or synchronized broadcasting.
If task services are unavailable the user sees a failure and must reopen the
template after reconnecting; no hidden request is retried.

The task panel receives a fresh task ID, clears prior confirmations and output
selection, opens its create panel, and preserves the full definition in an editable
JSON draft. It shows the adopted selector/group/count and allows changing target
selection. The ordinary save action freezes current host/profile revisions and
resolves targets using the existing TaskConnection.create path. The resulting
immutable plan must be reviewed and explicitly started through existing controls.
Opening or approving a template never creates a saved run or starts SSH execution.

The complete-definition editor provides an initial advanced editing entry. Dedicated
Invocation/variable forms, reusable check templates, parameter permission workflows
and QuickJS integration remain later work; this decision does not close all P2 scope.

## Deferred verification

Follow-up: ADR-0045 implements the dedicated typed editor and initial check/report
templates for this flow, preserving the complete-definition JSON import/export.
The later-work statements above describe the original slice before that follow-up.

This slice is source implementation and formatting only. Compilation, automated
tests, audit/MSRV, schema recovery, native GUI/IME and real SSH remain in the unified
verification phase. Verify version 1 byte compatibility, version 2 promotion and
sticky version, old-client read/write rejection, CAS/unknown reply handling,
RunScript shape/definition/selector/size validation and Debug redaction. Verify
cross-window/focus invalidation, full Invocation/custom-shell/variable/environment
round trips, draft editing, task-service failure, current catalog revision freezing
and absence of any save/start/broadcast when merely using a template.
