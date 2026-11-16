# SQLx SQLite compatibility patch

Source: the unmodified `src/` and license files from the published
`sqlx-sqlite 0.9.0` crate (MIT OR Apache-2.0).

The only code-package change is the normalized Cargo manifest's
`libsqlite3-sys` constraint: `>=0.39.0, <0.40.0` instead of
`>=0.30.1, <0.38.0`. SQLx upstream already supports `<0.40.0`:
https://github.com/launchbadge/sqlx/blob/main/sqlx-sqlite/Cargo.toml

The narrower lower bound requires the fixed SQLite version for
RUSTSEC-2026-0459. Cargo.lock pins the actual registry source/checksum of
libsqlite3-sys; the vulnerable C implementation is not vendored or relabeled.
SQLx 0.9.1 is not published as of 2026-11-16. Remove this patch when a compatible
published SQLx release permits the fixed dependency. Database and upgrade
retention tests must pass before distributing.
