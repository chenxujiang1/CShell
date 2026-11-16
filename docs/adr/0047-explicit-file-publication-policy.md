# ADR-0047: Explicit identical reuse and backup replacement

- Status: Accepted
- Date: 2026-10-11

## Frozen policy and durable receipt

P2-070 adds optional publication_policy to upload/download definitions and optional
FilePublicationReceipt to native target rows. None retains the original exclusive
no-overwrite publication. Optional None fields are omitted when encoding, preserving
ordinary legacy document bytes, hashes and SQLite schemas 13–15. New enum values
are skip_identical and backup_replace; mixed file steps retain the frozen leaf policy.

Before creating staging, inspect the original destination, reject collisions with
the generated backup name, and persist its size/SHA in the existing Creating
checkpoint. The receipt's previous content is immutable. It records separately
whether an identical destination was reused or an independent backup was verified.
Receipt flags are monotonic and only become successful with the committed native
target checkpoint; policy/content validation applies to commit and explicit adoption.

Source transfer, staged verification, durable Publish intent, publication ACK and
cleanup remain distinct. SkipIdentical still transfers/verifies source staging and
rechecks the destination; it reuses only exact content and leaves existing attributes
unchanged. Different existing content fails. An absent destination uses the original
exclusive publication path even with an advanced policy.

## Independent backup and replacement

BackupReplace preserves a fresh independent copy named
.cshell-backup-<run-id>-<host-id>.bak beside the destination. Copies use exclusive
creation, private permissions, bounded streaming, flush/fsync and full SHA readback.
They are not hardlinks to an inode another writer may later mutate. Backup collisions
fail closed, and backups are never automatically deleted by staging cleanup.

Recheck the original destination and verified backup immediately before replacing
the verified temporary file, and hash destination/backup again after publication.
SFTP requires the existing verified hardlink/fsync extensions plus advertised
posix-rename@openssh.com version 1 for replacement. It encodes the extension's two
path strings and requires a successful status; unsupported servers fail before
staging mutation. Local downloads use same-directory filesystem rename after staged
handle/content checks. Temporary cleanup treats a renamed-away owned temporary as
absent and retains the backup.

Neither filesystem/SFTP rename provides content-conditional compare-and-swap.
BackupReplace explicitly authorizes replacement under externally coordinated
exclusive destination access; the GUI states that requirement before saving/starting.
Checks reduce races but cannot guarantee exclusion of unrelated writers. The policy
does not claim an atomic multi-host transaction or roll back successful other hosts.
Failure after any backup/publication mutation remains uncertain and preserves the
original durable Publish intent and all artifacts.

## Unknown and explicit repair

Unknown publication is never resent or resumed. Existing readonly probe and explicit
adoption continue to bind exact target/parent versions. Adoption first checks expected
final content, then requires the original backup content when replacement was needed.
Identical reuse can be recognized separately. It only commits the verified native
receipt; mixed continuation still requires a fresh Pending confirmation/start.
Cleanup removes only the owned temporary, never destination or backup.

Download reopen permits an existing destination only for a frozen advanced policy.
The planned previous digest is retained across resume and rechecked before publish.
Upload probes permit the larger original destination under advanced policy; source
staging remains bounded to its declared source length. Existing adapters without
advanced support reject the policy through safe default port methods.

## Capability and UI

Minor 42 / FILE_PUBLICATION bit 46 is independent of OUTPUT_WORKSPACE. Request
shapes and operation numbers are unchanged. Creation checks the new capability
before sending advanced definitions. Authenticated daemon routing rejects advanced
stored file/mixed operations without the negotiated bit, including start/recovery;
ordinary definitions and list shapes remain compatible.

Upload, download and mixed builders expose all three policies and their frozen
reviews show the selected policy. Builder/source manifest changes invalidate the
previous save confirmation. CLI can edit the existing definition JSON's optional
publication_policy and uses the same daemon task engine. SQLite row JSON/CAS/events
persist the new receipts with no new table or migration; receipt bodies preserve
actual file outcomes and never create command exit codes.

## Verification deferred

Only code and formatting were performed. Compilation/tests/audit/MSRV and real
filesystem/SFTP/GUI acceptance remain in the unified verification stage. Verify
legacy byte/hash compatibility, policy capability checks, immutable/monotonic row
metadata and SQL rollback, absent/equal/different destinations, source changes and
limits, independent backup bytes/permissions/collisions, extension refusals, external
writer races, cancellation/timeout/crash/lost ACK at every substep, partial backups,
rename/reparse identity checks, exact-version adoption and mixed continuation.
