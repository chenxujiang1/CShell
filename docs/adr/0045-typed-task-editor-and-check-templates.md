# ADR-0045: Shared typed task editor and check templates

- Status: Accepted
- Date: 2026-10-10

## Shared editor

P2-050 task creation, mixed command steps and P2-030 RunTask/RunScript templates use
one desktop TaskDefinitionEditor. It loads existing TaskDefinition fields directly
and builds definitions through the existing validator and shell compiler. It replaces
the separate script-only forms while keeping the existing frozen-plan, save and
explicit start paths.

The editor supports POSIX, PowerShell, CMD and custom launcher/dialect; ordered
Script or Invocation steps; ordered literal or variable-reference arguments;
variables/environment; success-code sets, idempotence and all execution policy
fields. Switching command/argument modes retains the other mode's transient input
but encodes only the selected mode. Empty literal arguments stay present. Script
text remains static and is never interpolated. CMD restrictions and custom launcher
validation remain authoritative in the application compiler.

Variables/environment are editable rows rather than an immediately updated map.
Empty/duplicate names block definition construction, so a duplicate does not silently
overwrite a previous value. Removing or renaming a variable does not rewrite its
references; missing references are shown and block saving. Invalid success-code
text is retained for correction. Limits follow the task engine: 32 steps, 256 arguments,
64 variable/environment entries, 8 KiB values and 64 KiB compiled script limits.
Frontend character limits are convenience bounds; byte/resource validation still
occurs before creating/saving definitions.

Step, argument and key/value rows have local widget identities that remain stable
when reordered. Imports and template adoption create fresh row identities. These
keys are transient and do not enter domain documents or saved task fingerprints.

Script, program, literal argument, launcher, variable/environment value and JSON
draft buffers use Zeroizing. Debug reports counts and revision with a redacted body.
This is not a password store; saving task templates remains ordinary configuration
with the existing persistence acknowledgment.

JSON import/export remains available. Export materializes the current valid form.
Import is bounded to 1 MiB and requires explicit replacement acknowledgment; invalid
JSON/definitions or unsupported mixed/script shape leave the current form untouched.
Valid import loads every typed field rather than reducing Invocation to script.

Mixed command definitions remain one step with concurrency 1 and no child batch/
canary/gate/failure policy; the parent controls these. Mixed mode exposes child
timeouts and rejects incompatible imported policies instead of silently discarding
them. RunScript permits one Script step with ordinary task policy. Ordinary tasks
allow ordered multiple steps. Draft revisions advance on edits/import/template use
and participate in mixed create-confirmation invalidation, including invalid drafts.
Quick template edits reset the persistence acknowledgment.

## Built-in checks and reports

The application exports BuiltinTaskTemplate with stable slugs and fresh-definition
factories. The first six are POSIX file existence, directory existence, systemd
service active, disk usage report, PowerShell file existence and service running.

Checks are ordinary command steps using success code 0; they retain actual native
exit codes and outputs. A failed check blocks later mixed steps through the existing
command receipt rules. They do not create synthetic success, introduce a new Check
receipt, bypass canary/gates or automatically rerun Unknown results. Templates
declare idempotence for these read/check operations; users must review any edits.

POSIX dynamic paths/service names are variable arguments quoted by the compiler.
PowerShell templates read explicit environment values and use literal path checks
or exact service-name comparison. Example paths/service names must be edited and
remote tool availability/permissions reviewed before saving. Shell is specified
by the template and never inferred from platform tags.

Template adoption requires confirmation and only changes the local draft. Users
can replace a definition or append steps to a valid ordinary task with the same
Shell. Append preserves the current name, identity, policy and existing steps.
Variable/environment names merge only when absent or equal-valued; incompatible
values, Shell mismatch, resource limits or invalid compilation reject the entire
append without altering the draft. Mixed command/RunScript cannot append steps.

Custom reusable definitions continue to be saved as RunTask/RunScript actions in
the existing catalog. QuickJS/parameter permissions remain later automation work.

## CLI and compatibility

cshell-cli task template-list lists slugs and labels.
cshell-cli task template <slug> prints a validated TaskDefinition with a new ID.
Both operations are offline and do not discover/connect to daemon, resolve hosts,
save a run or execute a command. The printed definition can be edited and passed
to the existing task create command with a separate selection and catalog revisions.

Existing domain documents, hashes, SQLite schemas and IPC capabilities are unchanged.
The editor produces the existing typed definitions; ADR-0044 catalog version/capability
rules still apply when saving quick task templates.

## Deferred verification

Implementation and cargo fmt only; compile/tests/audit/MSRV/native GUI and real
SSH checks remain in the user-requested unified verification phase. Cover full
definition round trips, custom Shell/Invocation/empty and multiline arguments,
ordering/mode caches, duplicate rows/missing references/codes and byte limits,
JSON import atomicity, mixed/RunScript policy preservation and rejection, draft
revision acknowledgment invalidation, all template compiler paths and offline CLI.
Verify check success/failure and command absence/tool errors on actual POSIX/
PowerShell targets, append conflict/atomicity, no implicit save/start/broadcast,
and the existing native receipt/Unknown/canary/retry behavior.

This slice does not close P2: aggregate indexes/search, text alignment diff, file
replacement/backup policies, archive bundles/extraction/retention and P2-099 remain.
