# ADR-0054: Dedicated SSH tunnels and bounded recovery

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-12

## Scope

Complete the remaining dedicated-session/recovery coding after ADR-0053. A tunnel
can own a private SSH transport and its full jump chain, without a PTY, terminal
pipeline or batch task transport. Closing its reference interactive tab does not
close this private transport. GUI/CLI configuration, review, start, stop and clearing
are included. CA management and Provider extensions remain P3-020 work.
Compilation, tests, native UI and real networking remain deferred by request.

## Freeze first, authenticate separately

ConfigureDedicated requires a Stopped record, exact tunnel catalog revision,
explicit confirmation, a live verified reference SessionId and a configuration
containing its saved ProfileId, exact Profile catalog revision and optional retry
policy. The daemon takes a consistent Profile snapshot, resolves/clones the entire
eight-hop-bounded route and compares host/account/route with the reference peer.
Existing peer binding must match. It persists the complete plan and captured peer;
it neither authenticates nor creates a listener.

The frozen plan contains ordinary connection metadata and credential/key-file
references, not secret values. References are resolved at authentication time;
later Profile edits do not replace the frozen records. GUI review displays the
saved route, auth kinds, metadata revision and policy. StartDedicated is a separate
confirmed operation and can run after the reference tab has closed. It saves a new
run and synthetic private transport identity before starting background work.

Every hop and the target use strict known_hosts plus the captured SHA-256 key pin
in the pre-authentication server-key callback. Pins supplement current CA,
certificate/principal/time and revocation checks; they do not bypass them. Complete
peer identity is checked again before listener creation. Direct/single-hop trust
serialization is preserved by the shared route-identity helper.

## Native lifecycle and recovery

The current Vault/desktop authentication generation is captured at explicit start.
Cancellation and generation changes stop permit waits, authentication, listener
creation, running work and retry delay. Final authentication commit checks the same
generation. Unlocking later does not resume a stopped run.

At most four private paths authenticate concurrently. Active/unknown/waiting
tunnels still consume the existing 32-record budget; each private path is distinct
and holds its own transport and jumps. Each opening attempt has a 300-second
deadline including permit wait, authentication and listener creation; the listener
request also retains its 30-second bound. Connection metadata still obeys the
private 1 MiB catalog bound; some legal large plans may hit that byte cap before
the 128-record cap.

Retry is opt-in, disabled by default: at most five retries per explicitly started
run, 1–60 seconds delay, and a ten-minute recovery window measured from its first
recoverable failure. Counts do not reset on a successful retry. A successful tunnel
can continue running after the window; a later failure then requires explicit start.
Only network connection failures or unexpected worker/transport termination are
eligible. Configuration/trust/credential/authentication refusal, authentication
deadline, listener-create uncertainty and storage failures do not trigger replay.

No forward request is sent during a pre-listener connection failure. Authenticated
intermediate paths are confirmed closed before such a network failure becomes
retryable. For an established tunnel, the old forward task must finish and the
private leaf plus all jumps must be confirmed closed before another attempt.
Forward stop retains its join handle across cancellation, aborts after five seconds
and allows two seconds to join. Private path close has a twenty-second proof bound;
uncertain closure blocks automatic recovery. Close proofs refer to local worker/
transport termination, not a remote server compare-and-swap or proof that its TCP
cleanup was instantaneous.

Each retry saves its intent/count/fresh transport ID before delay or new network
work, keeps the frozen definition/peer and rechecks operation ownership. Stop is
bound to that run and prevents further dispatch. Fixed local ports are rechecked
by native bind; no alternate-port fallback is introduced. Explicit zero ports may
receive a different acknowledged port after reconnect, shown in the new receipt.

Read/restart does not create owners or run authentication. Orphan work becomes
Unknown; a manual Stop clears a lost private binding as metadata recovery and
cannot cancel a different interactive/new transport. The private guard attempts
cleanup on cancellation/drop. Unknown is not an automatic retry source; after
explicit resolution, starting again creates a fresh reviewed run.

## Storage and protocol

Optional/default-omitted dedicated plan, owned-run flag and retry count extend
the version-1 record, preserving old documents' canonical bytes/checksum. No SQLite
migration or dependency version changes. Validation checks the full frozen Profile
graph, reference/config IDs/revision, policy bounds and state/count consistency.

Minor 48 / DEDICATED_TUNNELS bit 52 extends Tunnel operations:

| Operation | Effect |
|---|---|
| ConfigureDedicated (6) | Save reviewed Profile/peer/route/policy; no authentication |
| StartDedicated (7) | Explicit new private run from the saved plan |
| ClearDedicated (8) | Stopped plan only; retain existing peer binding |

Existing envelope tags 49/50 remain. Advanced operations require the new capability;
old clients cannot read or modify a catalog containing dedicated fields. New clients
can still use basic operations against an older daemon with TUNNEL_CONTROL, but
advanced operations fail negotiation without fallback. Replies bind exact tunnel
ID, revision, saved configuration/reference and effect state. Background GUI reads
do not restore authority after uncertain writes. Catalog/profile/session/form
changes clear relevant GUI acknowledgments; revision accessors avoid cloning the
full Profile catalog just to display a revision.

## Deferred verification

Only source review and cargo fmt ran. Unified checks must cover old byte identity,
capability rejection and wire shapes; frozen graph/peer/config drift; target and
each hop pin refusal before password/signature authentication, CA/revocation;
permit/deadline/cancel/Vault races; retry count/window/delay, fixed and allocated
port behavior; bridge abort/join and transport closure proofs; late ACK/storage
failure, orphan/restart/metadata cleanup; GUI/CLI scope/authority/dirty configuration;
actual dedicated local/remote/SOCKS and multi-hop connections on supported platforms.
Blocked local storage calls remain subject to native filesystem verification;
Tokio cancellation cannot terminate a blocked kernel fsync.
