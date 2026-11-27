# Fork changes

Modified October–November 2026, based on upstream Moonlight Web v2.10.0 (commit `cd9d03cbf9a42b394f7b72a733a2f39cb5f0edd8`).

- Restyled the web interface and updated desktop input, keyboard controls, and stream settings.
- Added optional clipboard integration and streaming diagnostics.
- Updated WebRTC video pacing and related telemetry.
- Added a Docker build that compiles both server binaries and the web interface from this source.

This source snapshot combines the maintained server and streamer sources with the November 22 TypeScript frontend. The private deployment uses additional static asset overlays and site configuration. No prebuilt Docker image or binary is distributed by this repository.

The original program and the modifications are distributed under GPL-3.0-or-later. The `LICENSE` file contains the GPL v3 text. Pretendard is distributed under the [SIL Open Font License](web/resources/fonts/OFL-Pretendard.txt).

## Build this fork

From the repository root, build using the fork's source Dockerfile:

```sh
docker build -t moonlight-web-fork -f docker/Dockerfile.source .
```

For a local development build, use Node.js 24 and the Rust toolchain pinned in `rust-toolchain.toml`, with a C/C++ toolchain, CMake, Clang, and OpenSSL development headers.

```sh
npm ci
npm run build
cargo build --release --locked
```

The upstream Docker instructions in the README install the upstream release. The command above builds the fork instead.
