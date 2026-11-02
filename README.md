# udiskr
[![AUR Version](https://img.shields.io/aur/version/udiskr?logo=archlinux)](https://aur.archlinux.org/packages/udiskr)

This is a [`udisks`](https://www.freedesktop.org/wiki/Software/udisks) wrapper providing functions to automatically mount and unmount devices,
which can be used as a lightweight alternative to [`udiskie`](https://github.com/coldfix/udiskie).

## Install
```sh
cargo install --git https://github.com/uriib/udiskr.git
```
The compiled binary will be installed at `$HOME/.cargo/bin/udiskr` by default.

Builds on **stable Rust** (no nightly required).

## Usage

Run `udiskr` command to start the daemon.

It's recommended to add `udiskr` to startup applications.
```hyprlang
exec-once = exec udiskr
```

## Features
### Available
- Automatically start `udisksd` (daemon for `udisks2`)
- Automated mounting of devices
- Automated unmounting when devices are unplugged
- Desktop notifications (with click-to-open)
- Wayland and X11 support for opening file manager
- Minimal memory usage
### Planned
- Encrypted device support

#### Note
I'm currently working with a `dbus` implementation in `zig`,
`udiskr` will be immigrated in the future.
