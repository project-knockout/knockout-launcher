# Project KNOCKOUT Launcher

Source code for the Project KNOCKOUT launcher on Windows and Linux.
This repository contains the launcher and the assets needed to build it.

## Build

Install Rust and Cargo, configure the build environment, then run:

```sh
cargo build --locked --release
```

The executable is `target/release/dko-launcher` on Linux or
`target/release/dko-launcher.exe` on Windows.
On Linux, building requires a C/C++ toolchain, pkg-config, and X11 development
libraries for the launcher window. Playing requires a supported Divine Knockout
installation and Steam/Proton. Building does not require the game files.

To cross-compile Windows from Linux, install the MinGW-w64 x86-64 compiler,
linker, and resource tools, then run:

```sh
rustup target add x86_64-pc-windows-gnu
cargo build --locked --release --target x86_64-pc-windows-gnu
```

The result is `target/x86_64-pc-windows-gnu/release/dko-launcher.exe`.
The build embeds `assets/favicon.ico` using Windows resource tools.

## Source layout

- `src/main.rs`: launcher interface, installation, downloads, verification,
  updates, protocol links, logging, and game launch.
- `src/process.rs` and `src/process_windows.rs`: platform-specific game discovery
  and launch handling.
- `src/launcher_update.rs` and `src/game_manifest.rs`: download manifest
  formats and validation.
- `src/windows_setup.rs`: setup helpers.
- `src/p2p/`: a peer-to-peer bridge that replaces the Steam bridge the retail game used.
- `assets/` and `build.rs`: embedded graphics, font, and Windows resources.

The Rust package retains its existing MIT license declaration. The bundled
Noto Sans font has its own license in `assets/NotoSans-LICENSE.txt`.
