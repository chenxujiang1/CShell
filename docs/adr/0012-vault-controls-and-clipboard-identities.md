# ADR-0012: Vault controls, locking and automatic clipboard identities

- Status: Accepted
- Date: 2026-09-30

## Credential modes

The daemon owns `credentials/master.vault` beside the Profile database, in a separate protected directory. Use the existing per-user Windows DACL implementation or Unix mode 0700; a protection failure blocks credential access. The desktop never links the Vault backend, receives private-key bytes or derived keys, or persists credential input.

Offer two explicit ways to create an encrypted Vault:

- Master password: ADR-0011 Argon2id and AES-256-GCM format; the user supplies the unlock password.
- System keychain: generate a 32-byte OS-random root secret in the daemon. Store it under an opaque UUID in Windows Credential Manager, macOS Keychain or Linux Secret Service. Apply the same Argon2id key separation and encrypted record format. A bounded 24-byte `.root` sidecar contains only the root reference. Unlock reads the root secret from the keychain; no password or keychain fallback is available for this mode. Removing its keychain entry makes it unavailable.

The `.enabled` activation marker persists the user's opt-in. An activated missing/corrupt Vault or unavailable root key blocks new saved SSH authentication, including native password entries, external keys and agent authentication. Restart cannot silently downgrade to the native-only workflow. Root metadata is persisted before the first system Vault file, so an interrupted first creation can be recovered by explicit Reload when the activation marker is absent. A missing activated Vault is never recreated as empty. Unknown writes require refresh/reload, with no automatic mutation replay. System files cannot be unlocked on another machine without their OS keychain root; encrypted export is P1-090 work.

Per-Profile credential-store selection is an encrypted kind-4 record, using `profile-credential-mode-v1` and one byte. Missing selection means the existing native keychain/external-key workflow, after the global lock gate passes. Selecting encrypted Vault means passwords, passphrases and imported keys must come from that Vault; missing records never fall back. Bind encrypted credentials to a versioned JSON tuple of Profile target, account, authentication method, key/certificate references and route. Changes require resaving/reimporting. Mode changes do not migrate or delete existing native entries.

Private-key import reads the saved path only in the daemon, bounds it to 64 KiB and validates OpenSSH decoding and its optional passphrase before storage. Batch writes atomically insert key/passphrase or remove an obsolete passphrase. Removing an imported key also removes its passphrase and retains encrypted mode. The source file is retained. Decryption and SSH key use stay in the daemon.

## Lock and transient input

IPC minor 16 adds VAULT_CONTROL bit 19, VaultRequest/Response envelope tags 41/42 and AUTOMATIC_CLIPBOARD bit 20. Requests carry only an operation, optional transient password/passphrase, opaque Profile reference and revisions. Secret request buffers clear on drop; nested Debug hides them. Lock-generation CAS prevents a queued unlock arriving after lock from reopening the Vault. Profile store mutations and saved SSH creation require Vault capability once activated, protecting older clients from mode confusion.

Opening a file starts locked. Explicit lock, failed unlock, reload and expired unlock lease discard derived keys. The daemon's absolute lease lasts 15 minutes, checked both on access and by a weak timer. Workbench focus loss clears transient inputs and uses a separate authenticated connection for urgent lock, even while the Profile worker is awaiting Argon2. This intentionally locks when switching applications or native windows. Focus loss also disables keyboard/IME terminal input. This conservative cross-platform focus policy is the P1 lock-screen policy; it is not a claim that native OS lock notifications or disconnected IPC can synchronously revoke daemon memory. The lease bounds delayed/missing focus notifications. Established SSH sessions continue.

Lock generations cancel authentication, jump host scans and PTY creation already in progress. Final session registration and invalidation share a short commit gate, so a completed lock cannot publish a new session from an old generation. Returned secrets in canceled blocking work still clear when that bounded worker finishes; locking cannot revoke memory already returned to an independent caller.

The password widget has no plaintext undo history or galley/accessibility text. Preallocated buffers bound reallocation; edits support committed Unicode/IME, paste, cursor movement and deletion without copy or undo. Render a fixed mask and clear egui text/paste/IME events after processing. Close, focus loss and a locked-state transition clear UI inputs. Native OS input processing still handles transient events; this is not a memory-forensics guarantee.

## Persistent automatic OSC52 writes

Policy format v2 adds an optional automatic list; v1 remains valid only without grants. Each grant binds canonical host/port, SSH username, the actually verified peer's SHA256 key fingerprint and a captured versioned route. Jump routes include jump endpoint/account/route and its actually verified fingerprint. Profile edits do not change a running session's identity. Host-key rotation, account or route changes do not match old grants. Only a running verified selected session can introduce a new grant; manual hostname input only adds blocks.

The GUI displays the fingerprint/account and a risk acknowledgement before saving. Default remains ask. Host blocks and automatic grants cannot overlap; adding a block revokes grants for that host. All changes use the existing independent SQLite policy revision, limits and CAS. Storage failure or a conflicting save restricts current and newly created sessions until a successful policy Load or Save. Clipboard polling cannot clear this restriction by reading an old policy after failed revocation. Grant changes invalidate pending content and old OSC fragments. Removing a grant returns to ask for future requests.

Polling returns an automatic flag with the existing bounded, expiring token, never clipboard contents. The focused GUI requests the distinct ApproveAutomatic operation for the selected source session. The daemon reloads current policy, checks the grant, consumes the token once and returns text. The GUI checks source selection and focus again before writing. It retains at most 64 attempted automatic tokens for 31 seconds, including across session switches, and will not replay them after unknown responses or evict still-valid attempts to make room. A full deduplication cache suppresses automatic attempts until entries expire. Existing 16 KiB, one-pending-request, 30-second TTL, five-second cooldown and disabled remote reads remain enforced.

## Diagnostics and verification

Extend ADR-0009/0010 redaction to LocalPtyError, StorageError and JournalError: Display/Debug/Error.source expose fixed categories, numeric recovery state or I/O kind, never backend text. Keep typed variants for recovery. Journal records, output queues, LocalProfile and submission failures hide content/arguments/environment in Debug. Post-commit index failure and rollback failure retain their recovery meaning.

Validate hostile batches, mode persistence/binding, lock expiry, lock during Argon2, stale unlock generations, missing activated files, system-root reopen/unavailability, actual SSH authentication after removing the imported source file, automated egui input/redaction, verified-session automatic grant/save/approval/revocation and forbidden identity substitution. Run workspace fmt, strict Clippy, tests/benches/doctests, dependency review, MSRV and opt-in native Windows keychain checks. The existing native CI job also opts in to keychain tests on other desktop targets; a local Windows pass does not substitute for its current-head result. Native lock-screen and subjective UI/IME use remain usage acceptance, distinct from the automated engineering gate.
