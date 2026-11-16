# ADR-0025: Desktop task review and explicit retry

- Status: Accepted
- Date: 2026-10-08

## Decision

IPC minor 25 adds TASK_PLAN_CONTROL (bit 30) for Review and Retry. Both also
require TASK_CONTROL. Review returns the immutable snapshot and current revision
only on an explicit request; ordinary status/list replies continue to omit commands.
Retry accepts a saved parent revision, explicit Host IDs and confirmation, creates
a new queued run from the original frozen plan, and never starts it. Unknown targets
require separate confirmation and every step must be idempotent. Succeeded and
nonterminal targets are rejected. The parent must be terminal and have no active
runner. No host/Profile edit is adopted by a retry.

The desktop owns a bounded IPC worker and short-lived view state. Creation uses
the loaded inventory revisions. Review shows frozen destinations, shell, policy
and compiled commands; Start and gate approval bind the displayed run revision.
Replies lost after mutations disable further execution authority until explicit
reload. No mutation is replayed automatically. Closing the desktop does not cancel
daemon-owned execution.

The first result console provides virtualized target/step states and explicit
64 KiB stdout/stderr pages rendered by the existing wgpu LogSurface. Pages are
byte-addressed and escape controls/non-ASCII bytes without interpreting remote
terminal sequences. Paging is manual; a page is not a full log or completion proof.
Full text indexing, merged streams, search and exports remain P2-060 follow-up work.

## Implementation status

This iteration is code only at the user's request. Compilation, tests, native GUI
checks and acceptance are deferred to the later consolidated verification pass.
