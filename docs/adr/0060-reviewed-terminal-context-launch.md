# ADR-0060: Reviewed terminal context launch and optional recording

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-13

## Native consumers

Continue ADR-0059 / P3-040 by connecting terminal settings to new saved sessions.
The Profile authority captures a validated Profile catalog, Host catalog and
terminal policy catalog, resolves the selected context on a blocking worker, and
builds the connection/program plan from that same Profile snapshot before reading
credentials or opening a PTY/network connection. Resolution and all referenced
versions are frozen for that launch; subsequent edits do not change the plan.

An explicit reviewed launch submits the complete TerminalResolutionReport. The
daemon requires confirmation, exact Profile/Host/policy revisions, selected
Profile/Host relationship and exact recomputed report equality, including metadata,
matched tags, final values and every field source. Unknown theme references fail.
The report has a 1 MiB limit and the receipt is bounded before native creation.
Host context only affects scalar settings; the saved Profile retains its address,
program, route, authentication and credential references.

SSH uses the resolved terminal type in request_pty; the backend-neutral connection
exposes open_terminal_with_type with bounded nonempty/control-free input and PTY
capability checks. Existing callers retain open_terminal's xterm-256color default.
Local launches inject the resolved child-only TERM environment variable after the
validated environment layers, remove TERM case aliases and enforce the combined
environment count/byte limits. A conflicting transient LocalLaunchOptions TERM is
rejected with an instruction to use the terminal context. The saved environment's
TERM is subordinate to this explicit scalar terminal setting.

## Recording disabled

Logging=true retains the segmented CSJR journal, timing sidecar, ordered persistence
before parsing, paging/search/raw replay and existing error behavior. Logging=false
creates neither journal writer nor timing writer; it does not open/recover/delete
any existing output file. Input queues, parser, clipboard observer, terminal responses,
frame publication, resize and bounded backpressure remain active. This is not an
additional file export setting: it controls daemon output recording for this session.

A disabled JournalLineIndex is an empty memory object that never inspects its path.
Paging, styled paging, stored-history search and raw slices return RecordingDisabled.
The live terminal and its ordinary bounded terminal history remain usable; no disk
history or raw replay is claimed. Current triggers consume the journal, so arming
them requires recording and fails explicitly for disabled sessions. Frame highlights
are independent. Session details state when recording is disabled.

Existing sessions keep their initial mode; no live toggle, retroactive recording,
recovery of discarded bytes or implicit journal enabling is added. The disabled
mode governs terminal output files, not unrelated Profile, task or policy metadata.

## Protocol, compatibility and clients

Minor 54 adds TERMINAL_CONTEXT_LAUNCH bit 58 and envelope tags 61/62. A dedicated
request carries report JSON, dimensions and confirmation; the response carries the
standard session creation result and exact captured report. Both clients check
request identity, status, session UUID/Profile/title and the full receipt. Creation
is an explicit action, not an automatically applied change on policy save or read.

Ordinary SessionCreateRequest for a saved Profile also uses saved tag policies,
TERM and Logging when the new feature is negotiated; it has no Host or temporary
context. Unnegotiated saved creation retains its legacy TERM/recording behavior.
Platform-default sessions and old internal constructors remain unchanged. The
desktop requests the new feature; explicit reviewed creation refuses unsupported
daemons without substituting legacy creation. The production handshake now
advertises already integrated Phase 2/3 capabilities, which had been omitted from
main's manual feature list, including policy and reviewed launch capabilities.

The policy window provides a separate new-session checkbox/button bound to the
resolved form, report and revisions. It uses the existing pending workspace launch
slot; identity/receipt is validated before binding the new session's theme context.
This does not relaunch the current tab or persist temporary overrides. Multi-window
theme sharing retains ADR-0059 identity/version invalidation.

CLI terminal-policies launch accepts a raw resolution report or kind=resolved
reply file, validated regular/bounded/BOM-compatible input, explicit rows/columns
and --confirm. It prints session UUID and captured resolution. GUI/CLI creation
has a 300-second deadline including network/authentication/PTY work. Loss of reply,
timeout or invalid receipt is an unknown outcome, with no automatic retry. Users
must inspect existing sessions before a new manual launch. This request has no
once-only persistent launch ledger or deduplication nonce; --confirm must not be
replayed by callers as an idempotent operation.

No new database schema, package versions or journal format changes are made. Policy
JSON and Profile/Host SQL are captured reads, not a cross-store atomic transaction.
The existing Profile mutation gate protects same-service catalog writers while
capturing, not external processes or independent Host writers.

## Deferred verification

Only source editing and cargo fmt were performed, per the user's coding-first
instruction. Compilation, tests, audit/MSRV, native GUI and actual SSH/local PTY
validation remain in the unified batch. Include wire negotiation/old constructors,
exact review metadata/source/version substitution, cross-store edits, invalid
dimensions/TERM/environment bounds, both PTY implementations and guardian behavior;
disabled writer/index paths with no files/sidecars, live parser/response/resize,
paging/search/raw/trigger refusal, enabled recording durability; lost/late replies,
deadline/cancellation/launch ownership, CLI input/receipt and GUI pending tab/theme
binding/secondary windows/confirmation clearing. P3-040 remains in progress for
advanced workspace recovery/separation.
