# Third-party notices

v6cast is licensed under GPL-3.0-only (see [LICENSE](LICENSE)). It includes or links the
third-party software below. Each entry names the license that applies to that component;
the full license texts ship with the sources.

## Vendored source code

These two crates are copied into `vendor/` with a few marked patches (see
[vendor/README.md](vendor/README.md)). Their license files are kept in the vendor folders
and must ship with any binary or source distribution.

| Crate | Version | License | Copyright | License file |
|-------|---------|---------|-----------|--------------|
| cros-codecs | 0.0.6 | BSD-3-Clause | The ChromiumOS Authors | [vendor/cros-codecs/LICENSE](vendor/cros-codecs/LICENSE) |
| cros-libva | 0.0.12 | BSD-3-Clause | The ChromiumOS Authors | [vendor/cros-libva/LICENSE](vendor/cros-libva/LICENSE) |

## Direct dependencies of `v6cast-media`

| Crate | Version | License | Feature |
|-------|---------|---------|---------|
| openh264 | 0.9.10 | BSD-2-Clause | default (software H.264) |
| pollster | 1.0.3 | MIT OR Apache-2.0 | default |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | default |
| tracing | 0.1.44 | MIT | default |
| wgpu | 30.0.2 | MIT OR Apache-2.0 | default |
| cros-codecs | 0.0.6 | BSD-3-Clause | `vaapi` (vendored, see above) |
| gbm | 0.15.0 | MIT | `vaapi` |
| pipewire | 0.10.1 | MIT | `pipewire` |
| ashpd | 0.14.3 | MIT | `pipewire` |
| tokio | 1.54.1 | MIT | `pipewire` |

Where a crate offers a choice ("MIT OR Apache-2.0"), the notice above covers both options;
you may use either one.

Other crates in the dependency tree (transitive) are not listed one by one here. Their
licenses can be read with `cargo metadata` or `cargo tree`. The project's audit found that
all of them are permissive (MIT, Apache-2.0, BSD, ISC, Zlib or Unlicense).

## openh264 and patents

The software H.264 encoder is built from the openh264 source bundled with `openh264-sys2`
during the build; no prebuilt binary from Cisco is downloaded or linked. Cisco's patent
licence for H.264 covers only the prebuilt binary that Cisco distributes, so it does not
apply to this build. Anyone who distributes a binary that uses H.264 should check the patent
position for their own case.
