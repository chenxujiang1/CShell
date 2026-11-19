# CShell patch for russh-sftp 3.0.0

Source: [crates.io russh-sftp 3.0.0](https://crates.io/crates/russh-sftp/3.0.0), archive SHA-256 `093197e526668d92bba562e2bbbe98d1af9831bf080b619c736316ca1fa35101`, source commit `c2776c64c27e554dda0e0304925f890833fea5b1`. This directory includes the library sources, upstream README and Apache-2.0 license. Upstream examples, benchmarks and their development dependencies are not included.

In response to [RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543.html), replace `chrono` with standard-library Unix time conversion and Jiff UTC formatting without optional timezone databases. This preserves SFTP v3's unsigned 32-bit seconds and `%b %d %Y %H:%M` longname format. No authentication, transfer or packet handling is changed.

Remove the patch when a maintained upstream release no longer depends on `chrono` and passes CShell's SFTP metadata and interoperability checks.
