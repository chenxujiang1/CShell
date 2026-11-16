# ADR-0059: Terminal context policy inheritance

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-13

## Scope and ordering

Continue P3-040 with scalar terminal settings policies and explicit context inspection.
Resolution order is global defaults, root-to-leaf folders, matching tag policies,
saved Profile overrides, selected Host overrides, then temporary overrides. Each
field retains its own typed source. Null means inherit; false explicitly disables
logging. Tags match Unicode lowercase values, with rules applied in ascending
(priority, policy UUID) order. Profile tags drive matching; Host tags are metadata.
A Host context must reference the selected Profile. It never replaces the saved
connection address, route, authentication, credentials or local program.

The existing Profile resolver and ADR-0058 inspection retain their three-layer
behavior. Extended contexts use a separate typed API and negotiated capability.
Only terminal type, theme reference and logging participate. No list, environment,
program, credential or network inheritance is introduced.

## Storage and confirmation

cshell.terminal-policies v1 permits at most 64 tag policies and 256 Host policies,
256 KiB canonical JSON. IDs are nonnil and unique, tag/priority pairs cannot be
ambiguous, priorities range 0–1000, tags are at most 64 bytes and text overrides
at most 128 bytes. Proposal validation checks known theme and Host references.

The daemon stores runtime/terminal-policies/catalog.json in a private checksummed
directory using the existing no-follow/reparse, lease, revision CAS and synced
atomic-replace pattern. An absent file yields revision 1; corruption fails closed.
No database schema or dependency version changes are made.

Preview binds the exact proposed catalog, Profile and Host revisions, and a sorted
potentially associated Profile list, at most 2,048 entries. This list is a scope
summary, not a claim that every final field changes. Explicit resolution reports
show actual values and sources. The daemon retains at most 16 nonzero nonce previews
for five minutes. Apply requires confirmation, all three exact revisions and a
SHA-256 of the full typed review. A matching token is consumed once Apply reaches
the claim step; stale/noncanonical requests can be rejected before that step.
Restart invalidates previews. Unknown results are never automatically replayed.

The policy service serializes its own requests and holds the Profile mutation mutex.
Only policy JSON publication is atomic. Profile/Host SQL and JSON are not a combined
transaction, and the mutex does not exclude independent processes or Host writers.
Reference versions describe captured reads. Later changes require fresh resolution;
missing or mismatched contexts fail rather than selecting a different Host/Profile.

## Clients and display usage

Minor 53 adds TERMINAL_POLICIES bit 57 and envelopes 59/60, with Get, Preview,
Apply and Resolve operations. Authenticated desktop access and capability are
required. Requests reject noncanonical unused fields; replies are bounded to 1 MiB
and carry explicit scope/version/effect proof. Resolution uses a blocking worker.

Desktop provides bounded JSON policy editing, a generated tag rule example,
preview/scope confirmation, explicit reread and Profile/Host/temporary inspection.
Background two-second reads do not restore write authority after an uncertain
Apply. Changed drafts, epochs, nonce or versions invalidate confirmation; a saved
acknowledgement only replaces the unchanged submitted draft. Desktop recomputes
resolved values/sources against the captured Profile and policy catalogs.

Saved tag policies participate in desktop Profile theme display. Explicitly
applying a resolved display context binds TabId, SessionId, ProfileId and all three
catalog revisions. A bounded UI-owned shared map retains bindings across window
transfers; changed identity/version invalidates the binding. Temporary context is
never written into the persisted workspace or restored on restart. Keyboard/IME,
search, accessibility and synchronized input guards include the policy window.

This initial slice applies display themes. Native TERM/logging consumers are
subsequently connected by [ADR-0060](0060-reviewed-terminal-context-launch.md).
Existing PTYs and network destinations are not changed. Advanced workspace
recovery/separation remains P3-040 work, so the whole task stays in progress.

CLI supports get, preview-to-new-file, apply --confirm and exact-version resolve.
Regular bounded inputs accept UTF-8 BOM; synced private preview publication refuses
existing output paths. Editing an exported review changes its digest and cannot
authorize the server's original retained preview.

## Deferred verification

Only source editing and cargo fmt were performed. Compilation, tests, audit/MSRV,
native GUI and real transport checks remain in the user's unified validation batch.
That batch must cover precedence/Unicode/ties/null/false/source identity; old resolver
compatibility; storage corruption/lease/CAS/atomic publication; cross-store races;
nonce limits/expiry/restart/review tampering/unknown replies; UI dirty and late epochs,
version invalidation/window transfer/input isolation; CLI boundaries and receipts.

Usage: [TERMINAL_POLICIES](../phase3/TERMINAL_POLICIES.md).
