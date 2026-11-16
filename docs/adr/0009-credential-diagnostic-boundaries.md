# ADR-0009: Credential diagnostic boundaries

- Status: Accepted
- Date: 2026-09-30

## Decision

- Treat host-key confirmation tokens as temporary credentials in both directions of IPC. Debug output for previews exposes only revision-independent metadata, never the token or peer-supplied text. Wipe preview tokens and queued confirmation tokens when released; use volatile zeroization for credential requests, handshake tokens and IPC encoding/decoding buffers. Discovery records hide instance tokens in Debug and wipe tokens and temporary read/write buffers when released.
- SSH adapter errors keep their typed variants for retry classification and handling. Display and Debug expose fixed failure categories and bounded numeric metadata, without raw backend messages, key parser text or arbitrary string payloads. Error source chains stop at this boundary so generic error reporters cannot recover those payloads by traversing sources. Existing wire status codes and user retry semantics stay compatible.
- known_hosts I/O diagnostics expose only the I/O kind; validation still exposes the offending line number. Host-key scans return a fixed failure when the peer closes before offering a key. Terminal event Debug shows event kind and byte counts without terminal output or exit-signal text.
- Profile response and desktop Profile state Debug expose revisions, presence flags and counts. Imported text, environment values and error details are omitted, including in enclosing IPC envelopes and UI state.
- Redaction applies to diagnostics. Terminal output and public host-key fingerprints remain available through their explicit product interfaces. This change does not provide encrypted private-key import, master-password Vault mode, lock-screen policy or persistent automatic clipboard trust.

## Validation

Use sentinel secrets in nested IPC previews, backend errors and terminal events; verify Display, Debug and source-chain traversal. Retain codec round trips, native keychain tests, strict host-key tests and real SSH/IPC confirmation tests. Diagnostic output must retain failure categories while preventing payload leakage.
