# ADR-0062: PuTTY registry session metadata import

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-13

## Public format and implementation boundary

Start P3-050 with exported PuTTY session metadata. The primary public reference is
the [PuTTY 0.85 manual](https://the.earth.li/~sgtatham/putty/0.85/htmldoc/Chapter4.html):
§4.1.2 identifies the Sessions registry subtree and states that saved sessions are
independent of Default Settings; §4.33 describes registry-file export. The SSH,
authentication and proxy panels document the corresponding user-facing behavior.

Public persisted field names were checked against published PuTTY source mirrors
([settings identifiers](https://raw.githubusercontent.com/github/putty/master/settings.c),
[Windows storage identifiers](https://raw.githubusercontent.com/github/putty/master/windows/winstore.c)).
These are reference facts, not imported code. CShell's bounded Rust tokenizer,
decoder, conversion and UI/CLI were independently written. No commercial importer
code, vendor binaries/assets or source files were copied into the workspace. The
first slice does not assert full compatibility with every PuTTY release or extension.

The parser is an application-layer converter with no filesystem, registry, environment,
keychain, subprocess or network access. Input is a user-selected .reg export of
HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions, not a live-registry scan.
The root section may be empty; each immediate session subkey supplies one entry.
Other namespaces, deletion directives, default unnamed values, binary/expandable/
multi-string values, continuation records and unsupported syntax reject the batch.
This intentionally excludes a whole-PuTTY export containing other subtrees.

## Bounded scalar grammar

The input/decoded text limit is 1 MiB, with 32,768 lines, 8 KiB per line, 512 session
sections and 512 values per session. Accept UTF-8 with optional BOM, or UTF-16LE
with BOM and valid surrogate pairs. Arbitrary ANSI/code-page guessing and UTF-16BE
are unsupported. Headers are Windows Registry Editor Version 5.00 or REGEDIT4.
Blank lines and semicolon comment lines are accepted. Quoted REG_SZ text permits
only quote/backslash escapes; DWORD values require exactly eight hexadecimal digits.
Values and names are case-normalized for duplicate detection. Repeated session
sections, duplicate decoded names and duplicate scalar keys reject rather than
applying registry overwrite semantics.

Session names are percent-decoded once as bytes and must form valid UTF-8, nonempty
control-free names of at most 128 bytes. No decoded text becomes a path or program.
Raw input in the CLI and decoded/string-value parser buffers use Zeroizing; desktop
conversion clears its raw buffer before result/error handling. Diagnostics contain
fixed categories/line numbers/counts, not registry values or unknown field names.
The source .reg file is not persisted into a review or sent over IPC.

## Conversion and omitted behavior

Default Settings, non-SSH or unspecified protocols, and entries without a host are
skipped with counts; no defaults are inherited from the skipped section. A file
without any convertible sessions is refused. For candidate SSH sessions, map the
decoded name, HostName, PortNumber (default 22), UserName (or the caller's explicit
default account) and optional TerminalType. Host/account/port/TERM/name limits and
control checks are validated through the existing Profile catalog. Bare IPv6 is
supported; embedded usernames, URI/port syntax and bracketed hosts are refused.

Nonempty remote commands, environment/forwarding maps, logical host aliases and
authentication plugins, active proxies, no-PTY/no-shell, agent/X11 forwarding or
non-SSH-2 modes reject all candidate conversion. Missing inactive fields use the
bounded subset's defaults. The converter does not infer a direct route from an
active unsupported proxy configuration.

Each result gets a fresh Profile UUID, SSH kind, import:putty tag, root folder,
Direct route and Password authentication with no imported secret, key path,
certificate path or agent identity. PublicKeyFile/certificate references, manual
host trust and source logging settings are counted as omissions, not opened or
converted. PPK content/conversion, Pageant/authentication preference chains, packet
logs, themes, terminal/key mappings, trust overrides and algorithm preferences are
not imported. Remaining source values are omitted with a count and an explicit
fixed notice that CShell policies apply.

Converted Profiles explicitly set Logging=false. Import changes configuration,
not a running process. Native TERM/logging use follows ADR-0060 capability negotiation;
old unnegotiated session creation retains its legacy behavior. This metadata flag
does not promise that an old daemon honors the new recording consumer. New or
changed targets need usable CShell credential/trust bindings before opening. Existing
CShell credentials on an explicitly replaced Profile are governed by the existing
import/vault rules; this converter neither imports nor deletes them.

The conversion emits credential-free ProfileImportDocument v2 and an aggregate
PuttyImportSummary. Generated identifiers are frozen in the converted document;
reconversion may generate different IDs. Canonical document SHA-256 and the public
pure plan_profile_import wrapper support independent client review and effect proof.

## Desktop and CLI

The existing Profile import selector adds PuTTY .reg metadata and explicit default
account input. Conversion runs in the existing IPC worker, before Profile preview.
The UI displays bounded omission notices and requires their review alongside the
converted targets. Format/path/account/policy/revision checks remain; a converted
document digest is carried by CommitReviewedImport and compared to the worker's
retained bytes before submission. A new preview clears prior state, a changed digest
clears omission confirmation and legacy unbound CommitImport refuses PuTTY previews.
The retained client preview is consumed on commit/refusal, not automatically replayed.

CLI foreign-import convert-putty performs offline conversion to a new v2 file and
prints sanitized summary/notices. preview-putty captures an authenticated catalog,
uses the shared pure planner and writes a bounded 2 MiB review with the exact
document/digest/policy/revision, human rows and omission summary. apply --confirm
validates the subset and summary bounds, rereads the catalog, independently replans
and compares every row/change count, then computes the expected full catalog.
After the existing CommitImport transaction, it compares the normalized full receipt,
including retained fields, to that expectation. Conflicts/edited reviews/no-op imports
refuse; unknown writes are not replayed. Requests have a 40-second client deadline.

File reads are regular and bounded; JSON review files accept UTF-8 BOM. Publication
uses private synced temporary files and refuses an existing target. No original
secret-bearing .reg bytes are exported. Normalized collection ordering is only for
receipt comparison, not an alteration of stored layout or Profile semantics.

The daemon receives only the existing v2 document and uses existing conflict planning,
global revision CAS and one SQLite transaction. There is no new protocol feature,
minor/schema or dependency version. Existing PROFILE_CONTROL/PROFILE_CONNECTION_IMPORT
negotiation is required; there is no legacy import emulation. Authentication, commands,
connections and trust changes do not run during conversion/preview/commit.

## Deferred verification and remaining coding

Only source editing, public reference reading and cargo fmt were performed. No
compilation, tests, audit/MSRV, native GUI, registry access or real connection ran.
The unified batch must cover UTF-8/UTF-16/BOM/surrogate/code-page limits, percent
names/duplicates/case aliases, line/key/session budgets and raw diagnostic redaction;
scalar grammar/types/deletion/continuations/namespace refusal; skipped sessions and
all active-behavior refusals; account/IPv6/port/TERM/catalog constraints and no inherited
defaults/secrets/keys/trust; preview/digest/late replies/omission confirmation; conflict
Stop/Skip/Replace, retained credential rules/full catalog receipts/CAS/unknown results;
CLI publication and offline behavior; real user exports on supported platforms.

P3-050 remains in progress: Xshell session exports, SecureCRT and MobaXterm formats,
and any PPK support require further independently scoped implementation/reference
work. No claim of complete vendor migration is made by this scalar metadata slice.

Usage: [FOREIGN_IMPORT](../phase3/FOREIGN_IMPORT.md).
