# MuteMic

Windows tray app to control a selected microphone with a keyboard shortcut.

## Features

- **System tray:** left-click to open settings; right-click to toggle the microphone, open settings or exit.
- **Hotkey:** configurable key, **F8** by default. Holding the key toggles only once.
- **Keyboard modes:** Raw Input receives background keyboard events; compatibility mode checks the key's current state. Neither blocks the key in other apps.
- **Endpoint mute:** uses Windows endpoint mute and available hardware mute controls for the selected microphone.
- **Minimum volume (default):** reduces input to the device's minimum gain and restores the exact original channel levels, including after an app restart. Loud sounds may still pass.
- **Options:** sound feedback and automatic startup with Windows.

Only the selected microphone is controlled. Disconnecting it never switches control to another input.

## Build and run

Builds on Windows 10+ (x64) with the `x86_64-pc-windows-gnullvm` Rust toolchain and [LLVM-MinGW](https://github.com/mstorsjo/llvm-mingw/releases) (`ucrt-x86_64`, `bin` on `PATH`).

```powershell
cargo +stable-x86_64-pc-windows-gnullvm build --release --locked --target x86_64-pc-windows-gnullvm --bin mutemic
.\target\x86_64-pc-windows-gnullvm\release\mutemic.exe
```

Open Settings from the tray, select your microphone and save.
