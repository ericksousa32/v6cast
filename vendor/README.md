# Vendored crates

These crates are used only by the `vaapi` feature of `v6cast-media` (Linux).
They are the published crates.io sources, wired in through `[patch.crates-io]`
in the workspace `Cargo.toml`, with the small changes listed below. Every
changed spot is marked with a `v6cast patch` comment. Both crates are
BSD-3-Clause, Copyright The ChromiumOS Authors (see each `LICENSE`).

Why vendored: cros-codecs is the maintained Rust VA-API stack with an H.264
encoder that writes its own headers and a stateless decoder with a full parser
and DPB. It targets ChromeOS (minigbm, ChromeOS VA drivers). The published
versions do not build against current libva headers, and need a few fixes to
work with desktop Mesa. Upstream main (checked 2026-11-28) still pins
cros-libva 0.0.12, so there is no newer release to use instead.

## cros-libva 0.0.12

- `src/buffer/vp9.rs`: `VAEncPictureParameterBufferVP9` gained fields in
  libva 2.23; the initializer now ends with `..Default::default()`, so it builds
  with old and new headers. Without this the crate does not compile on Fedora 45.
- `src/lib.rs`: `#![allow(warnings, clippy::all)]`, so upstream lints do not
  break our `clippy -D warnings` builds.

## cros-codecs 0.0.6

- `src/backend/vaapi/decoder.rs`: the placeholder VA context created before the
  first SPS is 64x64 instead of 16x16. AMD VCN refuses 16x16 with
  `VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED`, and the library then panicked.
- `src/decoder/stateless/h264.rs`: Baseline streams (profile_idc 66, no B
  slices) get a reorder depth of 0, and an access unit delimiter finishes the
  current picture. Together with the next patch this removes up to 16 frames of
  decode latency (5 at 1080p) that the spec's inferred DPB size would add.
- `src/codec/h264/dpb.rs`: `Dpb::bump_all_if_low_delay` outputs pictures as
  soon as they are decoded when the reorder depth is 0.
- `src/lib.rs`: same lint allowance as above.
- The `test_data` directories and the `test/` and `ci/` folders were removed
  (about 3 MB); they are only used by the crate's own tests.

## Not patched here

- Desktop Mesa GBM cannot allocate NV12; `v6cast-media/src/vaapi.rs` allocates
  an R8 buffer and describes it as NV12 instead.
- Mesa's encoder leaves slice NAL headers at 0 and cros-codecs writes a plain
  Baseline SPS; `v6cast-media/src/codec/h264.rs` (`repair_vaapi_stream`)
  repairs the encoded stream.
