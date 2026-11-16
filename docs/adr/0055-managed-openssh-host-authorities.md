# ADR-0055: Managed OpenSSH host authorities and certificate inspection

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-12

## Scope

Continue P3-020 with managed host CA public-key policy and read-only OpenSSH host/
user certificate inspection. Existing certificate authentication, known_hosts
markers and cryptographic validation are reused. The client stores public policy;
user-certificate authorization remains the remote sshd's configuration. No CA
private-key custody, certificate signing or remote server administration is added.
Provider extensions remain further coding. Compile/test/native/real SSH verification
is deferred by the user's instruction; only source review and cargo fmt run here.

## Managed policy and storage

HostAuthorityId identifies a public key permanently. A record contains a name,
OpenSSH host patterns, canonical public key, SHA-256 fingerprint and Trusted,
Disabled or Revoked state. A catalog has unique IDs/fingerprints, at most 256
records and 2 MiB canonical JSON. Pattern input is bounded to 2,048 bytes and 64
patterns with a positive scope; hashed host names are supported. Nondefault ports
use the OpenSSH [host]:port lookup. Ed25519, RSA and NIST ECDSA public-key formats
are accepted; actual signature/algorithm compatibility still requires verification.
Private keys/certificate blobs are not authority public-key inputs.

Private runtime/host-ca/catalog.json uses version-1 checksummed storage, regular
no-follow/reparse checks, bounded reads, a file lease, revision CAS, private
temporary file sync/atomic replace and Unix parent sync on blocking workers.
Load recomputes each canonical key/fingerprint as well as the storage digest.
There is no SQLite migration, dependency upgrade or edit to the user's known_hosts.
Initial absence is an empty managed catalog; corrupt present data refuses use.

Trusted records add scoped cert-authority entries to the verifier. Disabled records
have no managed rule. Revoked public keys are globally rejected as raw server keys
and as certificate signing keys, including certificates otherwise allowed by legacy
CA entries. Global managed revocations are separate from host trust entries so they
do not make every unrelated host appear already enrolled for first-key import.
Legacy known_hosts trust and revocations continue to apply independently.

Key replacement requires a new record. Duplicate fingerprints cannot create another
scope record; edit the existing record's patterns. Revoked tombstones cannot be
removed directly: first explicitly review a state change clearing that revocation,
then review removal. Removing/stopping a managed rule does not edit external trust.

## Review and application

Save and remove require a read-only preview against exact catalog revision and ID.
Preview normalizes the public key, validates the proposed whole catalog and returns
canonical effect, fingerprint, cryptographically random 32-byte token and five-minute
expiry. At most 16 previews are retained. Apply requires explicit confirmation and
the exact typed fingerprint. Its token is consumed before effect/version/expiry
checks; uncertain replies or rejected stale reviews need read plus a new preview.
Apply sends once and replies with the catalog revision incremented once.

Effective-rule changes retire the Vault authentication generation and coordinate
with a shared trust read/write gate. Routed interactive, task/upload/download,
dedicated connections and jump scans hold a read guard across trust loading and
authentication; publication holds the write guard and retires stale auth generations
before/after persistence. First-key import rechecks effective policy and key
revocation under the same gate after its rescan. Display-name-only changes and
inactive-rule edits do not invalidate authentication.

Existing interactive transports are not reauthenticated. In-flight credential
registration cannot retain a pre-change generation. Dedicated runs observing that
generation stop and do not resume on unlock. Operators can explicitly close existing
interactive transports when immediate disconnection is required.

## Certificate inspection

Inspection accepts bounded public certificate text (64 KiB) and optional canonical
host/port context. Two blocking inspectors run at most. It reports host/user type,
serial, key ID, principals, valid-after/before, mathematical signature result,
time-window result, subject/signer SHA-256 and critical/extension names. Option
values are not returned. No connection or authentication is performed.

With a host context, the effective legacy-plus-managed verifier also checks host
type, principal, critical options, time, trusted signer and revocation. Signature
checking alone is not used as a trust decision. Trust and time facts use one captured
Unix timestamp. The report includes input SHA-256, target and managed catalog
revision; clients validate scope and the GUI invalidates displayed results after
input/context/catalog changes. External file changes still require explicit recheck.

## Protocol and GUI/CLI

Minor 49 / HOST_CA_CONTROL bit 53 adds envelope tags 51/52 and Get, PreviewSave,
ApplySave, PreviewRemove, ApplyRemove and InspectCertificate (operations 0–5).
Requests are canonical/bounded and older daemons reject feature negotiation without
fallback. Tagged application replies bind operation, ID, revision, proposed effect
and inspection input/context. Debug excludes public-key text, tokens and certificate
metadata values.

The desktop has an independent worker/bounded queue, toolbar entry, full policy
editor, preview/typed fingerprint confirmation, state changes/removal and certificate
pane. Revision/selection/form/preview changes clear acknowledgments; uncertain apply
revokes writes until explicit reload, and background reads do not restore them.
Terminal and synchronized input are isolated while the panel is open. CLI exposes
the same workflow and accepts bounded UTF-8/BOM JSON; output is ordinary private
configuration including a temporary one-use preview capability.

## Verification still required

Only source review and cargo fmt ran. Unified verification must cover protocol and
legacy rejection, canonical/hash/lease/CAS/corruption/storage faults, immutable IDs
and duplicate keys, preview expiry/consumption/unknown replies, all scope forms and
negative/hashed/nondefault-port patterns, managed/legacy overlap and global revocation,
pin/CA/signature/principal/critical/time combinations, auth publication/generation
races and first-key import, inspection resource bounds/input binding, GUI authority/
input isolation, supported crypto/platforms and actual OpenSSH certificate servers.
Local fsync and blocking crypto cannot be forcibly stopped by Tokio cancellation.
