# Installing TRust

You'll need Rust, a C compiler, and the matching [Lumen checkout](https://github.com/RubyMaelstrom/Lumen)
next to TRust at `../Lumen`. Use Lumen revision
`04cf52652738e813e025c3cf8aef80450d91a7f5`.

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
trust-desktop's file chooser (for `<input type=file>`) uses the XDG Desktop
Portal on Linux: run `xdg-desktop-portal` with a backend such as
`xdg-desktop-portal-gtk`, `-kde` or `-gnome`. Without a portal it falls back to
`zenity` when installed. The terminal browser has its own built-in chooser.
WebGL needs an EGL/OpenGL ES driver. Add `--no-default-features` to the build
command if you want the system allocator instead of mimalloc.

TRust keeps small-net credentials with owner-only permissions: Gemini client
identities (PEM certificate and key, plus `.scopes` files) in
`~/.config/trust/identities/`, Gemini capsule certificate pins in
`~/.config/trust/gemini_known_hosts`, and DICT AUTH credentials in
`$XDG_CONFIG_HOME/trust/dict-credentials.json` (default
`~/.config/trust/`). On Windows they live in `%APPDATA%\trust`. These files are
not encrypted; back them up like SSH keys. To reuse an existing Gemini
identity, copy its PEM file (certificate and key in any order) into the
identities directory; TRust offers it the next time a capsule asks.

For Windows x64 builds from Linux, install the `x86_64-pc-windows-msvc` Rust
target, [cargo-xwin](https://github.com/rust-cross/cargo-xwin), Clang/LLD, LLVM
tools, and Ninja, then run:

```sh
cargo xwin build --release --locked --target x86_64-pc-windows-msvc
```

The executables land in `target/x86_64-pc-windows-msvc/release/`.
Windows WebGL needs a driver providing `libEGL.dll`.
