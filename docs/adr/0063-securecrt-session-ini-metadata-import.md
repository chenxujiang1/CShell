# ADR-0063: SecureCRT session INI metadata import

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-13

## Reference and scope

Continue P3-050 with one explicitly selected SecureCRT session .ini file. The
primary reference is VanDyke's published [scripting guide](https://www.vandyke.com/support/tips/scripting/scripting_essentials.pdf),
the session-file example around printed pages 107–109. It exposes Hostname,
Username, Protocol Name, the hexadecimal SSH2 port, Emulation and counted
Description records. [Vendor import documentation](https://www.vandyke.com/support/tips/importsessions.html)
describes the relationship between session metadata and .ini files. The
[vendor FAQ](https://www.vandyke.com/products/securecrt/faq/index.html) gives further
configuration/migration context and the port-forward-filter field.

Only public field/representation facts are used. The bounded Rust reader and
CShell integration were independently written; no vendor parser, scripts, binaries,
assets or password-decryption implementation are copied, run or linked. This slice
does not claim full SecureCRT version compatibility. Xshell reference research did
not provide a sufficient primary field grammar in this turn, so it remains subsequent
implementation work instead of guessing a vendor file representation.

The application parser has no filesystem, registry, environment, subprocess,
credential, trust-store or network access. Callers explicitly provide a session name
and fallback account. Desktop/CLI derive the name from the selected filename stem;
there is no directory enumeration, implicit import of Default/Global configuration,
folder reconstruction, linked-session lookup or XML settings archive import.

## Closed INI subset

Accept UTF-8 with optional BOM or valid BOM-prefixed UTF-16LE. Do not guess ANSI or
support UTF-16BE. Input and decoded text are limited to 1 MiB, 8,192 total lines,
8 KiB per line, 1,024 fields and 1,024 lines in each opaque block. NUL and unsupported
control characters reject. Decoded text, UTF-16 words, scalar strings and caller raw
buffers use existing Zeroizing ownership; errors contain category/line, not values.

Records are S:"name"=literal text, D:"name"=eight hexadecimal digits, or
Z:"name"=eight hexadecimal digits followed by that many physical lines. S text is
literal: no quote stripping, backslash decoding, variable or path expansion. Z
payload lines are skipped exactly, including apparent S/D/Z syntax or executable
text; they cannot inject fields. Missing block lines or oversized counts reject.
Duplicate names are rejected after ASCII case normalization, across all three
types. Blank lines and #/semicolon comment lines outside blocks are accepted.
Other record types, section syntax and XML are refused.

Require explicit Protocol Name=SSH2 and Hostname, with typed strings and a valid
caller-supplied nonempty name. Default/Global filename stems refuse. Username uses
the file value or a caller's explicit fallback, not a local environment lookup.
[SSH2] Port is hexadecimal, defaults to 22 when absent, and must fit 1–65535.
Bare IPv6 is allowed; embedded accounts, URI/path/port syntax and bracketed hosts
are not. Existing Profile validation checks names, host/account limits and controls.

Nonempty execution/route families reject conservatively, including dormant saved
references: script, command, firewall, proxy, dependent session, forwarding, initial
prompt/expect-send/auto-logon/send-on-connect, environment, X11 and no-PTY/no-shell.
For these families, nonzero numeric values or nonempty blocks refuse. Empty strings,
zero/count-zero are inactive; firewall/proxy text None is accepted. Port Forward
Filter is a policy field, not a forwarding definition, and is omitted rather than
treated as an active tunnel. This conservative classifier is deliberately stricter
than a complete vendor settings evaluator; uncommon exports may be refused rather
than silently losing connection behavior. Unknown remaining fields are never executed
or interpreted as CShell actions.

## Metadata result and omissions

Create exactly one root SSH Profile with a fresh UUID, import:securecrt tag, Direct
route and Password authentication, with no secret, key/certificate path or agent
identity. Host, port, account and filename-derived name are the only mapped settings.
Emulation and source TERM are explicitly omitted rather than treating an emulator
name as an equivalent terminal-type string. Algorithm/authentication preferences,
appearance, clipboard, logging and trust settings are omitted; CShell settings apply.
The result explicitly sets Logging=false, which is configuration, not a live-session
toggle. Actual new-session behavior remains ADR-0060 capability-dependent.

The aggregate summary records omitted fields/block lines and key/certificate,
host-trust, emulation/TERM and logging field counts. It emits fixed notices without
unknown names, paths, values, passwords or block contents. Nothing decrypts source
passwords, opens external key files, imports host keys or calls a vendor process.
Existing CShell credentials on a replaced Profile remain governed by existing
Profile/vault rules; they are not copied from or deleted by this converter.

ForeignProfileImportResult and ForeignProfileImportSummary unify metadata converters.
The summary uses closed, distinct schemas with an untagged representation solely to
retain the existing PuTTY v1 review shape. SecureCRT adds a fixed summary format
marker; CLI checks source identity, metadata subset and count limits before replanning.
Generated IDs and metadata are frozen in the v2 document after conversion.

## GUI/CLI integration and commit

The Profile import selector gains SecureCRT session INI and uses the existing
explicit default-account input. It displays filename naming and omitted-setting
scope, target preview and omission-review confirmation. Both foreign formats now
share reviewed_metadata and CommitReviewedImport, which checks the captured document
SHA against retained bytes and the exact catalog revision. Re-preview clears state;
changed digests clear confirmation; unbound legacy commit refuses foreign previews.
The original .ini is cleared after conversion and is never sent over IPC or retained
in the review artifact.

CLI adds foreign-import convert-securecrt and preview-securecrt, with the existing
apply --confirm. Offline convert publishes a new Profile v2 file and prints notices.
Preview captures an authenticated catalog and uses the shared pure planner; Apply
checks document digest/source/summary, rereads/replans every reviewed row, computes
the expected whole catalog, commits via existing Profile CAS/transaction and compares
the normalized complete receipt. It does not connect to the imported target.

Existing PuTTY review files remain readable through their prior summary shape. The
review envelope stays v1, bounded at 2 MiB; input is a regular file with a 1 MiB limit.
Output uses synced temporary files without replacement. Catalog changes, unknown
results, no-op reviews and mismatched effects require fresh reads/previews; no commit
is automatically retransmitted. Existing 40-second CLI request deadlines remain.

No daemon wire fields/capabilities, protocol minor, database schema, package versions
or dependencies are added. Existing Profile v2 / PROFILE_CONNECTION_IMPORT / SQLite
transaction/CAS and source credential/trust boundaries are reused.

## Deferred validation and remaining work

Only public reference reading, source edits and cargo fmt were performed. Compilation,
tests, audit/MSRV, real user exports/native GUI and network checks remain in the user's
unified batch. Include scalar grammar/literal handling/duplicate types, UTF encodings
and limits, counted blocks containing apparent fields/scripts/blank tails, missing/
oversized blocks, protocol/name/account/port/IPv6 constraints, conservative execution/
route refusals and the filter exception; no source secret/trust/key/TERM import;
omission counts and redacted diagnostics; PuTTY old reviews and summary discriminator;
GUI format/filename/epoch/digest/confirmation, conflict policies and full receipt proof;
file publication and concurrent/late/unknown commits on supported platforms.

P3-050 remains in progress for Xshell, MobaXterm, PPK and broader vendor format coverage.
XML archives, folder trees and batch directory selection are not claimed by this
single-session INI slice. Usage: [FOREIGN_IMPORT](../phase3/FOREIGN_IMPORT.md).
