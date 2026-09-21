# Installing TRust

You'll need Rust, a C compiler, and the matching [Lumen checkout](https://github.com/RubyMaelstrom/Lumen)
next to TRust at `../Lumen`. Use Lumen revision
`328b4bbd1de1e93547cc74bebe08b98d97088eaa`.

From the TRust directory:

```sh
cargo build --release --locked
```

Run `target/release/trust <address>` for the terminal browser or
`target/release/trust-desktop <address>` for the desktop browser. On Linux,
you can install both with:

```sh
mkdir -p ~/.local/bin
install -m 0755 target/release/trust target/release/trust-desktop ~/.local/bin/
```

Make sure `~/.local/bin` is on your PATH. Install mpv for audio and video playback.
WebGL needs an EGL/OpenGL ES driver. Add `--no-default-features` to the build
command if you want the system allocator instead of mimalloc.

For Windows x64 builds from Linux, install the `x86_64-pc-windows-msvc` Rust
target, [cargo-xwin](https://github.com/rust-cross/cargo-xwin), Clang/LLD, LLVM
tools, and Ninja, then run:

```sh
cargo xwin build --release --locked --target x86_64-pc-windows-msvc
```

The executables land in `target/x86_64-pc-windows-msvc/release/`.
Windows WebGL needs a driver providing `libEGL.dll`.
