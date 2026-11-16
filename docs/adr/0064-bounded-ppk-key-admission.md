# ADR-0064: Bounded PPK key admission and Vault binding

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-13

Update 2026-10-14: [ADR-0067](0067-rsa-ppk-and-ring-signing.md) adds bounded RSA
PPK admission and ring-backed signing. The RSA exclusions below describe the
original slice; the new ADR defines the current RSA subset and separate decoder.

## Reference and existing implementation

Continue P3-050 with PPK private-key admission. The primary format reference is
[PuTTY 0.85 Appendix C](https://the.earth.li/~sgtatham/putty/0.85/htmldoc/AppendixC.html).
It documents the ordered outer records, clear public blob, encrypted/plain private
blob, integrity MAC and version-specific KDF. No vendor binaries or decryption
scripts are invoked, and no commercial password extraction is implemented.

The already pinned russh 0.63.1 delegates PPK parsing to ssh-key 0.7.0-rc.11 with
the ppk feature enabled. Local source inspection found that decode_secret_key
already recognizes PPK. This slice replaces implicit unrestricted admission with
an independently written bounded envelope preflight, then reuses the pinned library
for authenticated decryption and key decoding. It does not reimplement MAC, AES,
Argon2 or curve operations. No dependency versions are upgraded.

## Supported subset and limits

Support PPK versions 2/3 with Ed25519 and NIST ECDSA P-256/P-384/P-521, using none
or aes256-cbc encryption. Version 1, unknown algorithms/ciphers, DSA, Ed448 and RSA
reject before KDF work. The pinned dependency graph does not enable the RSA signing
feature needed for the local pair proof; admitting RSA while skipping that proof
would not meet this slice's contract. Broader algorithm support remains separate
implementation work, not an unverified promise.

The file is UTF-8, at most 64 KiB; an initial UTF-8 BOM and surrounding whitespace
are normalized. CR/LF/CRLF line endings are normalized into a Zeroizing buffer.
No UTF-16 or arbitrary code-page guessing is added. At most 4,096 lines / 4 KiB
per line are admitted. Only exact, ordered header names are accepted, so duplicate,
reordered, extra KDF fields or trailing records refuse. Comment stays private to
the decoder and is never included in the inspector output.

Public/private blocks have positive decimal line counts, at most 2,048 each,
bounded base64-only lines of at most 128 bytes. Aggregate encoded public data is
at most 8 KiB and private data at most 32 KiB. The pinned decoder checks actual
base64/binary structure and MAC. Version 2 requires a 40-hex-digit MAC; version 3
requires 64. Source field payloads never appear in error messages.

Encrypted v3 admits Argon2d/i/id, one to four lanes, one to eight passes, sufficient
minimum memory and at most 65,536 KiB. Memory multiplied by passes is capped at
262,144 KiB-passes. Salt is 8–32 bytes expressed as bounded hexadecimal. These
limits are applied before library decryption. V2/unprotected files reject v3 KDF
records. Unprotected keys do not accept a nonempty passphrase; protected keys require
an explicitly provided passphrase, at most 16 KiB.

At most two PPK cryptographic decodes run concurrently per process. A shared
semaphore permit covers pinned-library decrypt/MAC/decode and pair proof, and Busy
is returned rather than waiting or retrying. The permit remains in the blocking
operation if its async caller is cancelled; a still-running KDF cannot free budget
early and allow additional work. Library-internal KDF derivation may run more than
once, so the product cap is an admission bound, not a measured wall-clock guarantee.

## Integrity, identity and secret boundary

After library MAC verification/decoding, the decoded algorithm must match the
admitted header. A fresh random 32-byte local challenge is signed using an explicit
CShell namespace and SHA-256 SSHSIG, then verified against the declared public key.
Failure refuses the key. The signature/challenge and private components are not
exported or sent to a server. A successful inspector returns only version, admitted
algorithm, encryption flag, KDF parameters and public SHA-256 fingerprint.

An unencrypted file's MAC is a format-integrity check, not authentication of its
creator or authorization to trust a remote host. Key admission is not host-key
trust, certificate trust or permission to connect. PPK v2's file-integrity SHA-1
does not enable SHA-1 SSH authentication or the legacy algorithm policy.

CShell-owned raw/normalized input and passphrase buffers use existing Zeroizing
ownership. The pinned crypto library manages its own temporary buffers; this slice
does not assert complete library-internal memory zeroization or audit clearance.
Decrypted key objects have the library's existing secret lifecycle. SDK errors are
mapped to fixed PpkError categories, without source comment, base64, MAC, passphrase,
private bytes or untrusted diagnostic payloads.

## Runtime and Vault integration

SshPrivateKey exposes decode_private_key, retaining decode_openssh as a compatibility
alias. Every recognized PPK entering that factory must pass the new admission.
Vault import and encrypted credential-package rebinding use the common decoder;
existing callers through the alias also gain the guard. The Vault maps decoder
budget contention to Busy. It continues to use saved Profile key path, exact
catalog/Vault state and credential binding, encryption and invalidation rules.

Saved interactive, multi-hop, task and dedicated connection paths use the shared
ssh_route authentication builder. Private-key decoding now runs on a blocking worker,
with owned zeroizing input/passphrase copies. A PPK error other than PassphraseRequired
fails before a keychain passphrase lookup; a malformed or over-budget file does not
trigger credential fallback. A valid protected file can retrieve the explicitly
bound existing passphrase and re-enter the bounded decoder. Vault generation and
connection cancellation continue to gate later authentication/registration.

The existing desktop Vault import button accepts an admitted PPK at the saved path.
It shows the supported algorithms and distinguishes source key passphrase from the
Vault master password. Private bytes remain daemon-side. The source file is preserved;
import copies its original format only into the existing encrypted Vault, with the
existing target binding and optional separately stored key passphrase. It does not
convert keys into unencrypted files, delete sources or extract session-file passwords.

## Offline CLI and dependency changes

foreign-import inspect-ppk reads a regular file bounded to 64 KiB. An encrypted
file uses the explicit --passphrase-stdin flag; the secret is not a process argument
or environment variable. It reads one bounded UTF-8 passphrase through EOF, removes
one final LF/CRLF and preserves spaces. Inspection executes on a blocking worker
and prints only verified public metadata. No daemon discovery, Vault access, key
binding, output key file, authentication or network operation occurs.

The CLI adds an internal cshell-ssh dependency. cshell-ssh directly uses already
locked rand and zeroize; rand was already present as a dev dependency. Cargo.lock
changes only the internal dependency lists, with no package/version updates. No wire
fields, feature bits, protocol minor or SQLite schema change; minor remains 54.

## Deferred validation and remaining work

Only source editing, public format/reference and local dependency API reading,
and cargo fmt were performed. No real keys were opened by this coding run, no PPK
decode/KDF, build/test/audit/MSRV, native Vault, keychain or SSH authentication ran.
Unified validation must cover versions/EOL/BOM/UTF-8/order/duplicates/counts/MAC,
wrong/missing/empty passphrases, all KDF boundary/products/ciphers, supported/unsupported
algorithms and forged public/private pairs; random proof/error redaction, dual-slot
contention and cancellation/later blocking completion; route/package/Vault credential
bindings and generations; original-file retention/encrypted storage; offline CLI
stdin spaces/BOM/EOF/length/public-only output, old key formats and actual platforms.

P3-050 remains in progress for Xshell/MobaXterm, RSA PPK and broader format coverage.
The .reg/.ini metadata importers still omit external key references. This independent
private-key workflow requires a separate explicit CShell key binding. Usage:
[FOREIGN_IMPORT](../phase3/FOREIGN_IMPORT.md).
