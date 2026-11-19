# CShell patch for russh-util 0.52.0

Source: [crates.io russh-util 0.52.0](https://crates.io/crates/russh-util/0.52.0), archive SHA-256 `668424a5dde0bcb45b55ba7de8476b93831b4aa2fa6947e145f3b053e22c60b6`. The registry archive is the source of record: its VCS metadata identifies `3d09c20c52a542ffa327723d3a44cb1edd9b0ca4` in `russh-util` and marks its publishing checkout dirty. The package declares Apache-2.0, and the license text is included here.

In response to [RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543.html), use the already locked `web-time` for the WASM clock. Native platforms still use `std::time::Instant`. No SSH protocol code is changed.

Remove this patch when upstream no longer needs `chrono` on any target.
