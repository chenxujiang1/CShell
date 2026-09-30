# ADR-0007: Session clipboard write consent

- Status: Accepted
- Date: 2026-09-30

## Context

P1-080 requires consent before remote OSC 52 writes to the operating system clipboard. SSH and terminal parsing stay in the daemon; remote clipboard reads remain forbidden.

## Decision

- Negotiate CLIPBOARD_CONTROL independently in IPC minor 13. Unnegotiated requests fail closed. Only running, verified SSH sessions are eligible; local sessions remain disabled.
- Keep the terminal adapter's OSC 52 disabled. A bounded observer in the SSH parser worker captures only complete seven-bit OSC 52 writes to the clipboard selection (`c` or empty), strict base64 and UTF-8. It does not send terminal replies or access the OS clipboard. Reject queries, other selections, invalid encodings, cancelled and oversized sequences.
- Keep at most one 16 KiB decoded request per session, for 30 seconds using a monotonic clock. Capture at most one request per five seconds. Never overwrite a pending request. No consent or clipboard contents are persisted in configuration or workspace layouts.
- Poll returns only an opaque request ID and byte count. Approve/reject requires that exact ID on that exact SessionId. Approval consumes the request before returning text; replay, expiry and session closure fail. A lost reply is not retried. The desktop applies text only if the originating session is still selected and the window is focused.
- The prompt offers allow once, reject once, or block this session. Block discards the pending request and prevents further capture for that session. A new Shell has a new policy and request identity. GUI reconnection can inspect an unexpired pending request but cannot restore past approvals.
- IPC Debug output hides clipboard text and request tokens; transient payloads use zeroizing buffers. Existing journal semantics still retain original terminal output, including escape sequences; this is not a diagnostic log or clipboard approval replay source.

## Consequences

Remote output alone never modifies or reads the local clipboard. Persistent host trust and a complete policy editor remain later P1-080 work. Automated tests cover fragmented/malicious sequences, resource caps, expiry, one-shot decisions, capability refusal, closure and desktop stale-session rejection.
