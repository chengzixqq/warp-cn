# Building Warp for Windows (portable, no Visual Studio)

Warp can be built for the `x86_64-pc-windows-msvc` target without a full
Visual Studio installation, using a portable LLVM toolchain plus the MSVC CRT
and Windows SDK headers/libraries fetched by [`xwin`]. This is useful for CI
and for contributors who cannot (or prefer not to) install Visual Studio.

> This documents the OSS build (`warp-oss`). It does not require Warp's private
> channel config — `script/install_cargo_build_deps` degrades gracefully when
> that is absent.

## Toolchain

| Component | Purpose | Source |
|-----------|---------|--------|
| Rust (`x86_64-pc-windows-msvc`) | compiler | `rustup` |
| MSVC CRT + Windows SDK headers/libs | system headers and import libs | [`xwin`] (`xwin splat`) |
| `clang-cl`, `lld-link`, `llvm-rc`, `llvm-lib` | C compiler, linker, resource compiler, archiver | [LLVM release] (portable `.tar.xz`) |
| `protoc` | protobuf codegen (`prost-build`) | [protobuf release] |

None of these require an installer or touch the Windows registry / system
policy; everything can live in a self-contained directory.

## Environment

Assuming `xwin` was splatted to `$XWIN` and LLVM extracted to `$LLVM`:

```sh
# MSVC CRT + Windows SDK headers (clang-cl reads INCLUDE, like cl.exe).
export INCLUDE="$XWIN/crt/include;$XWIN/sdk/include/um;$XWIN/sdk/include/ucrt;$XWIN/sdk/include/shared;$XWIN/sdk/include/winrt"
# MSVC CRT + Windows SDK import libs (lld-link reads LIB).
export LIB="$XWIN/crt/lib/x86_64;$XWIN/sdk/lib/um/x86_64;$XWIN/sdk/lib/ucrt/x86_64"

# C compiler / archiver for *-sys crates (ring, aws-lc-sys, ...).
export CC_x86_64_pc_windows_msvc="$LLVM/bin/clang-cl.exe"
export CXX_x86_64_pc_windows_msvc="$LLVM/bin/clang-cl.exe"
export AR_x86_64_pc_windows_msvc="$LLVM/bin/llvm-lib.exe"

# protobuf compiler.
export PROTOC="$PROTOC_DIR/bin/protoc.exe"

# Portable Windows resource compiler. app/build.rs honours WARP_RC and uses it
# instead of locating RC.EXE through the registry (see embed_resource_file).
export WARP_RC="$LLVM/bin/llvm-rc.exe"

# Use LLD as the MSVC linker (also avoids picking up a unix `link` from PATH).
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="$LLVM/bin/lld-link.exe"
```

## Build

```sh
cargo build --bin warp-oss --features gui
```

The resulting `warp-oss.exe` is written to `target/debug/warp-oss.exe`
(or `target/release/` for `--release`).

## Notes

- **Linker name clash:** if a Unix-style `link` (e.g. from Git for Windows /
  MSYS2 coreutils) is ahead on `PATH`, it can shadow MSVC's `link.exe`. Setting
  `CARGO_TARGET_..._LINKER` to `lld-link` avoids the ambiguity entirely.
- **`WARP_RC`:** only needed for portable / registry-less SDKs. Contributors
  with a normal Visual Studio install can leave it unset; `app/build.rs` then
  falls back to the standard `embed-resource` path.

[`xwin`]: https://github.com/Jake-Shadle/xwin
[LLVM release]: https://github.com/llvm/llvm-project/releases
[protobuf release]: https://github.com/protocolbuffers/protobuf/releases
