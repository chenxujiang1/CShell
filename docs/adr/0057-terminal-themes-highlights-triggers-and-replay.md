# ADR-0057: Bounded terminal tools and read-only replay

- Status: Accepted (functional code written; verification deferred)
- Date: 2026-10-13

## Scope and ownership

P3-030 adds theme editing, JSON/TOML and clean-room XCS palette transfer,
versioned HighlightSet editing/transfer, scoped live-output triggers, raw-output
Hex browsing and independent ANSI replay/export. The existing v1 palette has
foreground/background/cursor/selection and 16 ANSI colors. Indexed 256-color and
True Color rendering remain the existing terminal/renderer's responsibility;
this change does not expand persisted palette fields or add font settings.

The new workspace packages are cshell-theme (palette conversion),
cshell-highlight (pure matching) and cshell-automation (live trigger matching).
Domain models contain patterns, styles and declarative actions. Application
models own document validation, scopes and exact reply effects. The daemon owns
private configuration and live execution; rendering owns a bounded highlighting
worker; the desktop owns an independent replay worker.

P3-060 owns QuickJS execution, script recording and script capability grants.
The Script trigger action produces a script-capability-required event/reference;
it does not run a program or read the vault. Bell is presented as visual window
attention. Notify is a live workbench notice; event history remains readable.
No OS audio backend or unattended script authority is introduced here.

The user's instruction defers compilation, tests, MSRV/audit, GUI, platforms and
real SSH validation. Only source edits and cargo fmt have run. P3-030 functional
coding is closed within this scope; no acceptance gate is closed.

## Theme editing and transfer

The desktop edits foreground/background/cursor/selection and all ANSI 0–15
colors with a swatch preview. Saving captures the palette and catalog revision,
validates an exact daemon theme preview, then uses the existing atomic Theme
commit path. Profile assignment is a separate explicit metadata change. Import
still previews the immutable converted document before committing its revision;
export captures a saved revision.

JSON and TOML use cshell.terminal-themes version 1, at most 128 themes/256 KiB.
TOML conversion uses the already locked toml_edit 0.25.13 parser with only parse
enabled, depth/node limits and typed JSON validation; export emits the same
palette model. No dependency versions are upgraded or new parser/writer package
downloaded. Theme names remain lowercase ASCII stable references; built-in
default/dark/light names cannot be replaced.

The XCS parser is original code based on the INI data shape observable in the
author's [public XCS sample](https://gist.github.com/isayme/4282742). The official
[NetSarang color-scheme repository](https://github.com/netsarang/Xshell-ColorScheme)
and [manual](https://www.xshell.com/docs/Xshell8_manual.pdf) provide format context.
No third-party source or palette asset is bundled. This supports named palette
sections plus [Names] count/nameN, UTF-8/BOM UTF-16, six-hex-digit RGB values,
normal and bold ANSI colors. Duplicate/unknown/unreferenced sections/keys,
incomplete colors and normalized-name collisions refuse the whole conversion.
This is a documented public subset, not all Xshell versions/formats.

Bright ANSI values map to 8–15; cursor defaults to foreground and selection to
ANSI blue. The independent text(bold) value is validated but is not a v1 palette
field. The UI discloses this mapping before submission. Conversion never invokes
the other client, reads credentials or evaluates commands.

## HighlightSet and rendering

HighlightDocument is cshell.highlight-sets v1, with names, Profile scopes and
ordered rules. Empty scope applies globally, including unsaved sessions. The
private combined configuration is cshell.terminal-tools v1, initial revision 1,
256 KiB, up to 16 sets/64 display rules (including trigger Highlight actions)
and 64 triggers; each scope has at most 128 nonnil Profile IDs. The matcher is
compiled even for disabled rules during validation, so invalid patterns cannot
be saved for later activation.

Patterns support literal or Rust regex, case sensitivity, at most 512 UTF-8
bytes, 256 KiB compiled/DFA budgets and nesting limits. Empty-text matches are
refused; zero-width results are discarded. A RegexSet batches candidate rules
and each compiled rule produces at most the bounded per-line total. At most
16 KiB per logical line and 128 matches are processed. Spans align to Unicode
grapheme boundaries and map wide/combining cells to complete display ranges.

One renderer worker has a four-item queue, bounded compiled/cache/result maps,
4 ms matching slices and deferred continuation. Row matching is reused when
text and rule identity are unchanged. Results bind session, source, generation,
page/visible anchor and rule identity. Log matching is restricted to visible
logical rows (at most 512), not every row in the paginated history. Cache uses
LineId for logs and row identity for live snapshots, with generation binding at
result delivery. At most 512 cached row texts and bounded frame spans survive.
Dirty source/style changes can redraw without re-running unchanged row matches.

Overlays do not mutate terminal cells: remote style/theme, user highlight,
search, selection, cursor/IME. Later rules override specified colors; underline
adds to the remote style. Live, paginated history and split panes use the same
overlay model. Trigger Highlight rules are displayed only while that session's
trigger run is armed; painting is pure and never dispatches trigger actions.

## Persistent configuration and trigger lifetime

The private runtime/terminal-tools/catalog.json envelope has a SHA-256 digest,
regular/no-follow/reparse checks, bounded reads, lease/revision CAS and synced
temporary replacement/Unix parent sync on blocking workers. Save validates the
entire document and expected version, stops old runs and publishes revision+1.
It does not arm anything. Imports replace only the highlight sets in a reviewed
draft; saved trigger definitions are retained until the explicit whole-draft
save. Highlight exports contain the saved snapshot, not unsaved edits.

An explicit Arm binds the existing live session UUID, frozen catalog revision,
its saved Profile scope, a fresh run ID and the current authentication generation.
Reading, restoring, opening a pane or restarting daemon never arms a trigger.
There are at most 16 armed sessions. A shared four-worker semaphore bounds raw
reads and matching; ownership remains with detached blocking work until it ends.
Stop/replace cancels/join-aborts the async dispatcher, so a detached pure read or
match cannot later send input. Vault/session restrictions or generation changes
stop the run; unlocking does not resume it.

The dispatcher starts at the next committed raw output sequence and matches
only future output. Stateful ANSI/OSC/DCS/control stripping and split UTF-8
assembly feed an explicit 8 KiB cross-packet context. Pattern matching describes
that decoded text stream, not the terminal's final cursor-rewritten screen.
Overlong context is bounded; arbitrarily long regex matches are not guaranteed.
No raw matched output is copied into action parameters or event messages.

Every rule dispatches at most once per output sequence; consumed fragments do
not replay. Cooldown is 1 s–1 h, per-rule rate 1–60/minute and per-session action
budget 120/minute. AppendText additionally requires explicit per-arm permission
and is consumed at most once per rule per arm, even when permission or dispatch
fails. This prevents output/input feedback from automatically rearming itself.
No newline is added and ambiguous input dispatch is never retried. commit_auth
fences synchronous input-queue dispatch against authentication changes. A queued
input acknowledgement is not proof that a remote command completed.

Highlight/Notify/visual Bell/Record produce bounded events. Script produces a
capability request for P3-060. The event ring retains 512 events; replies return
at most 128, monotonic IDs, an explicit lost-before cursor and armed run bindings.
Clients independently validate scope/effect and advance through received events.
The desktop skips preexisting events at first attachment, presents each new event
once per window and never performs AppendText/Script from polling or redraw.
Lost events are reported by the cursor; there is no durable event-history claim.

## Raw output, timing and replay

JournalLineIndex exposes a read-only sequence/offset slice from its existing
authenticated session paths. It copies location metadata under a short lock,
releases the lock, checks the original header/sequence/range and streams the
entire record CRC before delivering at most 64 KiB. Record size remains 16 MiB.
There is no path RPC, repair, unbounded record materialization or index mutation.
Each fragment rechecks its record, so large-record browsing has a bounded but
potentially repeated CRC I/O cost. Removed sessions cannot be recovered through
an arbitrary file path; local ANSI input is a separate explicit desktop action.

The existing CSJR/CSIX/CSLI layouts remain unchanged. A .cstime sidecar has CSJT
v1 plus fixed 24-byte sequence/monotonic microsecond/rows/cols/CRC entries. Timing
is written before raw append becomes visible. Raw data stays authoritative;
restart truncates orphan timing beyond the repaired journal and old missing
rows are sparse/invalid, not invented measurements. Captured sizes describe each
output record; resize-only events and input direction are not raw records.
Flush synchronizes timing, and missing/corrupt timing degrades to untimed replay.
No password, keyboard-interactive answer or terminal input is captured here.

Hex displays offsets, bytes and printable ASCII only. Replay feeds raw output
into its own bounded Alacritty engine, drops every response and has no transport,
clipboard observer, trigger engine, vault or native action dispatcher. Play/pause,
record step, 0.25–8x speed and reset/from-start seek are explicit. New timing
paces records; legacy/plain ANSI input uses a fixed untimed pace/step. Seeking is
cancellable and EOF ends seeking; stale read/parser epochs cannot replace a new
source. Preview size is capped at 200x512; it is a read-only preview rather than
an interactive terminal pane. Visual font/cursor fidelity remains in the GUI gate.

Local ANSI import is a frozen file snapshot at most 8 MiB. Export captures an
explicit sequence range, at most 4096 records/8 MiB/120 seconds, verifies every
fragment, and publishes only complete output. ANSI exports do not carry timing
or input. CLI publishes a private synced staging file without replacing a target;
the desktop uses the existing export publisher. Slow kernel filesystem work is
not made preemptible by an async timeout.

## Protocol and clients

Minor 51 adds TERMINAL_TOOLS bit 55 and envelope 55/56: Get, Save, Arm, Disarm,
Poll and RawJournal. Noncanonical unused fields are rejected. Get/Poll do not
execute actions. Save/Arm require explicit confirmation and exact catalog version;
RawJournal binds session/sequence/offset. Read-only queries negotiate the feature
and fail on older daemons; themes reuse existing THEME_TRANSFER/Profile protocols.
Replies are typed, bounded to 512 KiB and checked for exact requested effects.

Desktop workers keep input/parsing/matching/file I/O off the main UI thread.
Uncertain mutations disable writing until explicit Get; polling never restores
permission or retries mutations. Dirty drafts are not overwritten by late reads
or save acknowledgements. Window input/IME/search/accessibility/sync-input guards
include the tools window. CLI and desktop all operations are documented in
[TERMINAL_TOOLS](../phase3/TERMINAL_TOOLS.md).

## Deferred verification

The unified batch must compile all targets/features and cover JSON/TOML/XCS
limits/encodings/collisions/unknown keys; theme revision/effect and legacy bytes;
Unicode/wide/combining/dirty-row/overlap layering and cache budgets; regex bounds;
split UTF-8/ANSI and cross-record/gapped context; once-per-sequence, cooldown,
rate, one-text-per-arm, generation/cancel/ambiguous queue behavior; event loss,
restart/read-only behavior; CRC/segments/orphan timing/old untimed journals;
replay pause/seek/EOF/epochs and suppressed responses/OSC/automation; export
publication/deadlines; GUI keyboard/IME/font/color/cursor/split-pane fidelity;
and real local/SSH output under the existing performance/aging/native gates.
