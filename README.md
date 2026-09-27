# Willow Discord Bridge for Windows

A tiny Tauri notification-area app that self-mutes—and optionally self-deafens—Discord while Willow Voice is listening.

> **Attribution:** This project adapts the Discord RPC and shortcut-gesture work
> pioneered in [Hush](https://github.com/MatthysDev/hush) by
> [Matthys Ducrocq](https://github.com/MatthysDev). Hush's original MIT license
> and copyright notice are retained in `LICENSE`; see `NOTICE` for details.
>
> This is an unofficial community project and is not affiliated with or endorsed
> by Willow Voice or Discord.

## Features

- Close settings without stopping the bridge; it keeps running in the notification area until **Quit** is selected from the tray menu.
- **Hold Ctrl + Windows:** mute Discord until the shortcut is released.
- **Double-tap Ctrl + Windows:** keep Discord muted during Willow's locked mode.
- **Tap once while locked:** stop locked mode and restore Discord's prior voice state.
- Optionally play a Discord Soundboard announcement when dictation starts; self-mute happens first. Paste the sound ID in settings (for example, `1328911757753712702`).
- Optionally self-deafen Discord too, blocking incoming audio while dictating. When announcing, deafening waits up to five seconds for the sound to finish.
- Preserve a pre-existing Discord mute/deafen state instead of blindly unmuting.
- Configurable shortcut, gesture mode, deafen behavior, unmute delay, and start-with-Windows.
- Encrypt Discord credentials and OAuth tokens with Windows DPAPI.
- Direct Discord local RPC—no simulated Discord keypresses or internet-facing server.

## Footprint

Measured from the optimized Windows x64 release build on the development machine:

| Artifact/state | Measurement |
|---|---:|
| NSIS installer download | **1.96 MiB** |
| Standalone application executable | **4.48 MiB** |
| Fresh tray-only startup working set | **12.3 MiB** |
| Fresh tray-only private memory | **2.2 MiB** |
| Tray after opening and closing settings | **25.3 MiB working / 6.0 MiB private** |

The settings window uses the WebView2 runtime already supplied by modern Windows. Its helper processes exist only while settings are open and are destroyed when the window closes.

## How it works

Willow does not expose a Windows recording-state API. The Rust backend therefore observes the same physical shortcut as Willow using `WH_KEYBOARD_LL`, mirrors Willow's hold/double-tap state machine, and writes Discord voice settings through Discord desktop's local named pipe.

The tray process starts without a webview. Tauri creates the settings webview only when requested and destroys it when closed, keeping normal background usage small.

## Requirements

- Windows 10/11 x64
- Willow Voice for Windows
- Discord desktop—not Discord in a browser
- Microsoft Edge WebView2 Runtime for the settings window (normally included with Windows)

## Discord RPC setup

1. Go to <https://discord.com/developers/applications> and create an application.
2. Add the exact Discord account signed in to the desktop app under **App Testers** while the application is unapproved. Otherwise Discord returns `OAuth2 Error: invalid_scope` for the restricted RPC voice scope.
3. Under **OAuth2**, copy the **Client ID** and generate/copy a **Client Secret**.
4. Add the redirect URI exactly: `http://localhost`
5. Open Willow Discord Bridge settings and paste both values.
6. Select **Save and connect**, then approve Discord's prompt.

## Develop

Requirements: Node.js 24+, Rust stable, and the Tauri Windows prerequisites.

```powershell
npm install
npm run typecheck
npm test
npm run dev
```

The frontend is compiled by the native Go-based TypeScript 7 compiler. The tray, keyboard hook, gesture state machine, DPAPI storage, Discord IPC, and OAuth implementation are Rust.

## Build

```powershell
npm run dist
```

Output:

`src-tauri\target\release\bundle\nsis\Willow Discord Bridge_<version>_x64-setup.exe`

The installer is currently unsigned, so Windows SmartScreen may display a warning.

## CI and automatic releases

GitHub Actions caches npm, Cargo, Rust build artifacts, and the Tauri NSIS toolchain. Every push and pull request type-checks the frontend and tests the Rust backend. When a new `package.json` version reaches `main`, CI automatically builds the Tauri installer, creates the matching version tag and GitHub Release, and uploads the installer.

Use `npm version patch`, `npm version minor`, or `npm version major`, then push to `main`. The npm version hook synchronizes the Rust and Tauri manifests automatically.

## Limits

- Synchronization is based on the shared shortcut because Willow has no public recording-state event.
- Discord RPC requires Discord desktop and developer-application authorization. Soundboard playback uses Discord's undocumented local `GET_SOUNDBOARD_SOUNDS` and `PLAY_SOUNDBOARD_SOUND` RPC commands; the bridge looks up the sound's source server automatically. Playing it in another server may require **Use External Sounds** permission. Errors appear in the settings connection panel without preventing self-mute. A sound will not play if you were already deafened or if another sound was attempted in the last five seconds.
- If Discord's prior state cannot be read, the bridge fails closed rather than risk unmuting you.
- A hard process kill cannot perform graceful cleanup, although Discord normally reverts RPC-controlled voice settings when its controller disconnects.

## Attribution and license

This project contains work adapted from [Hush](https://github.com/MatthysDev/hush), created by [Matthys Ducrocq](https://github.com/MatthysDev). Hush established the core design of observing an existing dictation shortcut and controlling Discord through local RPC instead of injected keyboard events.

The upstream MIT notice is retained verbatim in `LICENSE`. See `NOTICE` for adaptation details and `UPSTREAM_README.md` for the preserved upstream documentation.
