# ADR-0010: SFTP diagnostics and cleanup state

- Status: Accepted
- Date: 2026-09-30

## Decision

- Extend ADR-0009 to SFTP errors. Display, Debug and source chains expose fixed failure categories, protocol status codes, I/O kinds and byte counts. Peer error text, paths and cleanup reasons are excluded from diagnostics. Typed errors remain available for explicit handling.
- IPC minor 15 adds optional cleanup path, cleanup location and destination-committed metadata to the existing transfer snapshot. These additive protobuf fields do not change SFTP_CONTROL or existing transfer state values. Older peers can still use fixed error detail; absent fields do not imply that a failed destination is unchanged.
- Transfer completion uses the actual result. Only a clean TransferCancelled outcome becomes Cancelled; cleanup failure remains Failed even after cancellation was requested. A successful commit followed by cleanup failure retains a committed flag and tells the user not to repeat the transfer.
- The file panel displays bounded cleanup paths through an explicit product interface, identifies remote versus daemon-local storage and prompts the user to inspect destination status before recovery. It performs no automatic deletion or replay. Recovered snapshots retain the same metadata across GUI reconnects.
- SFTP request, entry, response, transfer, adapter page and desktop-state Debug hide paths, peer text and error details. Wire directory listings and user-entered paths continue to serve the file panel.

## Validation

Verify sentinel redaction through Display, Debug, source chains, nested IPC and desktop state; exercise real SFTP protocol error packets. Cover clean cancellation, cleanup failure, committed cleanup failure, bounded recovery metadata, reconnect recovery and actual UI rendering. Preserve existing atomic upload/download and destination-safety tests. Full Vault and other P1-080 acceptance remain separate.
