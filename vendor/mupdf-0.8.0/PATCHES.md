# Local MuPDF wrapper patch

Source: the `mupdf` 0.8.0 crates.io release from
https://github.com/messense/mupdf-rs. The upstream AGPL-3.0 license is retained
in `LICENSE`. Native MuPDF still comes from the unmodified `mupdf-sys` dependency.

This directory contains the upstream library sources, manifest, README, and
license. Example and integration-test targets and their fixtures are omitted.

`src/device/native.rs` uses `align_of::<f64>()` as the conservative native-device
alignment limit on MSVC, whose C headers do not expose `max_align_t`. The
compile-time alignment check remains in place. Other targets keep the upstream
`max_align_t` check. No other library source changes are made.

Remove this patch and the root `[patch.crates-io]` entry when upgrading to an
upstream release with MSVC support. Cargo applies this override to source builds
and `cargo install --path .`; it does not apply to a published crates.io package.
