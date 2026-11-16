# ADR-0015: Portable terminal Theme palettes

- Status: Accepted
- Date: 2026-09-30

## Format and references

Use a separate UTF-8 JSON document: `format: "cshell.terminal-themes"`, `version: 1`, `themes: [...]`. Each record has a stable `name` and a `palette` with required `foreground`, `background`, `cursor`, `selection` and `ansi` fields. Colors are arrays of exactly three integer sRGB channels (0–255); `ansi` contains exactly 16 such colors, in normal/bright ANSI order. Reject floats, out-of-range channels, wrong dimensions, missing fields and unknown fields. The format contains no commands, files, URLs, fonts, scripts or credentials. Errors do not echo malformed input values.

Names are lowercase ASCII references of at most 64 bytes: start with a letter/digit and continue with letters/digits, hyphen or underscore. Names must be unique; reserve `default`, `dark` and `light` for builtins. Bound each document to 256 KiB and the entire saved custom catalog to 128 themes. Validate the combined catalog, including retained themes. Accept an empty theme document for an empty export; its import does not increment the revision.

Profile global/folder/Profile inheritance continues to resolve a Theme name. Import palettes separately and select them in the Profile or folder editor. Profile JSON v3 remains unchanged and exports names as references; it does not embed palettes. Unknown references keep the existing Dark fallback and display as unknown in the editor. Older clients retain themes on Profile changes but cannot render custom palettes. [The paper example](../examples/terminal-theme-paper.json) is an importable public sample and is checked in the application round-trip tests.

## Transaction, IPC and UI

Store palettes in schema v9's `terminal_themes` table, using the existing Profile catalog revision and consistent snapshot transaction. Upgrades from versions 0–8 create the table and advance the schema in the same migration transaction; back up existing databases before migration. Do not change the catalog revision during migration. Profile repository writes preserve palettes alongside all other records. Import Fail/Skip/Replace decisions apply by exact canonical name; Replace updates a palette, Skip retains it. Recheck the preview revision and apply the complete batch in one transaction. No schema migration is applied to a user's live database during development verification.

IPC minor 19 adds THEME_TRANSFER bit 23, PreviewThemes/CommitThemes/ExportThemes operations 12/13/14, typed ThemeData in ProfileCatalogData tag 7 and optional preview ThemeData in ProfileImportItemData tag 5. ThemeData packs each RGB value as a 24-bit integer and requires exactly 16 ANSI entries. Theme operations require Profile control and Theme transfer; no SSH capability is needed. Reject mutation/credential fields and local-context options. Export requires the saved revision and reuses bounded `export_json` plus the desktop's existing synced, no-clobber file publication.

The desktop import format selector adds Theme JSON; cache the exact bytes, conflict policy, base revision and format for commit. File/source/policy/revision changes invalidate the preview; each commit attempt consumes the cache and is never automatically replayed. Show incoming palette swatches, or the actual retained palette for Skip. The export content selector chooses Profiles or Theme palettes. Custom palette names appear in the Profile and folder Theme selectors. Import and Replace refresh the catalog so existing tabs adopt changes without changing workspace layout.

## Rendering

Carry the concrete palette in each pane's theme and geometry cache key. Apply it to default foreground, pane clear color, ANSI indexes 0–15, default cursor color and selection overlay (fixed alpha 0.5). Live terminal and cold log text use the same color resolver. For custom inverse text, use the palette background as default inverse ink. Preserve xterm's indexes 16–255 and application-supplied truecolor; explicit terminal cursor colors take precedence. Builtin palette behavior and search overlays retain their existing implementation. Theme palettes do not change the application chrome or copy terminal contents.

## Verification and remaining work

Tests cover format dimensions/ranges/names/limits, the importable sample, combined catalog limits, all conflict policies, actual retained preview colors, framed IPC capabilities/revisions/round trips, SQLite insert failure rollback/reopen, v8 backup/migration failure, legacy Profile changes preserving palettes, typed IPC bounds, actual cursor/selection vertices, ANSI/truecolor behavior and existing-tab palette replacement. Pointer-driven import/export tests exercise Theme commands and revision capture. Native four-platform CI and subjective visual acceptance remain separate; automated CPU geometry tests do not claim GPU visual acceptance.

Independently password-encrypted credential packages, optional OpenSSH export and P1-099 acceptance remain separate work.
