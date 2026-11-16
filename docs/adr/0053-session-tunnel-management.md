# ADR-0053: Durable session tunnel management

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-12

Update: [ADR-0054](0054-dedicated-tunnels-and-bounded-recovery.md) subsequently
completes the dedicated-session and bounded-recovery coding left open here.
The original interactive-session semantics and deferred verification remain.

## Scope and authority

Continue P3-020 with daemon-owned local, remote and dynamic SOCKS5 tunnel management.
Definitions survive restart, but native listeners never start from loading a catalog.
Start requires an explicit live SSH SessionId and its verified peer. This slice
reuses interactive transports, including ADR-0052 multi-hop routes, and does not
share short-lived batch task transports. Dedicated tunnel sessions, optional
reconnect policies, CA management and Provider extensions remain further work.
Compilation, tests, audits, native UI and actual forwarding are deferred by request.

## Definitions, limits and storage

TunnelId is a domain ID. TunnelDefinition carries name/kind, literal-IP bind address,
requested port (zero means allocate), target host/port, explicit non-loopback
acceptance, connection cap and SOCKS handshake timeout. Dynamic has no fixed target.
Loopback is the default; non-loopback refuses validation without acceptance.
Names/hosts are bounded, control characters rejected; each tunnel permits 1–128
connections and a 1–30 second SOCKS handshake. Definitions are ordinary private
configuration, not credential documents; Debug omits their contents.

The version-1 cshell.tunnels catalog contains at most 128 records and 1 MiB canonical
JSON. A private runtime/tunnels/catalog.json envelope stores SHA-256 and identity.
Read/write run on blocking workers using no-follow/reparse checks, regular-file and
length bounds, a file lease, catalog revision CAS, a private temporary, file sync,
atomic replace and Unix parent sync. No SQLite migration or dependency upgrade.

Stopped/Starting/Running/Stopping/Unknown records save run/session/peer, requested
configuration, last acknowledged bound address/port and bounded detail. Active and
Unknown records together consume a budget of 32 globally and eight per session;
old unresolved bindings must be explicitly stopped before budget is reclaimed.
Editing/removing requires Stopped and no current owner. Updating retains the peer;
using another peer requires a separately reviewed new definition.

## Intent, acknowledgment and ownership

All mutations require canonical request shape, exact catalog revision and explicit
confirmation. Create/update only save configuration. Start persists Starting with
new run ID and verified peer before opening a listener; a runtime reservation owns
the operation. Native acknowledgment is separately persisted as Running with actual
allocated port. Native acknowledgment does not promise destination reachability or
successful per-connection traffic. Remote bind reporting is the requested address
and acknowledged port; server GatewayPorts policy may affect actual exposure.

Stop persists Stopping before signaling the owner. It closes the listener and all
active bridges using the existing bounded forwarding primitives. Native completion
or successful cancellation becomes Stopped; uncertain remote creation/cancellation
becomes Unknown, never a successful receipt. Native start is bounded to 30 seconds,
normal shutdown to five seconds, followed by task abort with uncertainty retained.
Explicit server RequestDenied on start is a confirmed refusal, not a grant.

The owner continuously observes its forward task and original session. Session end
initiates shutdown; closing the GUI does not stop daemon-owned work while its SSH
session remains live. ForwardHandle adds a cancellation-safe wait over its retained
JoinHandle and an abort handle. Remote grant cleanup removes routing only after the
appropriate acknowledgment; a fixed-port precheck refuses a known existing route
before sending another remote-forward request.

Read converts orphan Starting/Running/Stopping into Unknown. A lost client reply is
resolved by explicit read, never retransmission. Stop on an orphan targets only the
original SessionId/peer and recorded bound port or nonzero requested port. Unknown
allocated remote ports require closing the original SSH session rather than guessing
a port. If that original session is absent, explicit Stop clears its old metadata
binding without creating/rebinding a transport or sending a new-peer cancellation.
That path is metadata recovery, not a new remote cancellation acknowledgment.

Managed remote starts reject overlap with active/unknown fixed bindings on that
session. An unknown auto-allocated grant blocks further remote starts on the same
session. Local OS bind enforces local port exclusivity. Session/run ownership and
catalog CAS prevent another operation from replacing a live definition.

## IPC and clients

Minor 47 / TUNNEL_CONTROL bit 51 adds envelope tags 49/50 and canonical operations:

| Operation | Effect |
|---|---|
| Get (0) | Whole bounded catalog; conservative orphan reconciliation |
| Create (1), Update (2) | Definition only, explicit save confirmation |
| Start (3) | Saved definition, current revision, explicit live session |
| Stop (4) | Current revision, signal/cleanup original binding |
| Remove (5) | Current revision, Stopped metadata only |

Old daemons fail capability negotiation; no legacy forwarding fallback is used.
Clients validate document shapes, mutation revision+1, exact tunnel ID/definition,
Start session and effect state. CLI sends once with --confirm. The GUI uses an
independent IPC worker and bounded queue, displays listener/peer/native state, clears
confirmation on revision/session/selection/form changes, and revokes writes after
an unconfirmed mutation until explicit reload. Background reads do not restore that
authority. Tunnel UI captures terminal/synchronized input while open.

## Verification still required

Only source review/cargo fmt ran. Unified verification must cover canonical wire and
old-client rejection; catalog locking/CAS/corruption/sync/rename/storage failures;
all native intent/ACK loss, late stop, canceled opens, orphan/restart and absent
session cleanup; dynamic-port uncertainty and remote overlap; resource caps and
bridge shutdown; multi-hop peer binding; GUI confirmation/epoch/input isolation;
CLI/BOM/limits; IPv4/IPv6 and non-loopback binds; actual SOCKS/local/remote forwarding
and GatewayPorts/remote-cancel behavior on Windows/Linux/macOS and external servers.
