# Willow Discord Bridge for Windows

A tiny Tauri notification-area app that self-mutes—and optionally self-deafens—Discord while Willow Voice or Wispr Flow is listening.

> **Attribution:** This project adapts the Discord RPC and shortcut-gesture work
> pioneered in [Hush](https://github.com/MatthysDev/hush) by
> [Matthys Ducrocq](https://github.com/MatthysDev). Hush's original MIT license
> and copyright notice are retained in `LICENSE`; see `NOTICE` for details.
>
> This is an unofficial community project and is not affiliated with or endorsed
> by Willow Voice, Wispr Flow, or Discord.

## Features

- Close settings without stopping the bridge; it keeps running in the notification area until **Quit** is selected from the tray menu.
- **Hold Ctrl + Windows:** mute Discord until the shortcut is released.
- **Double-tap Ctrl + Windows:** keep Discord muted during Willow's locked mode or Wispr Flow's hands-free mode.
- **Wispr Flow profile:** also supports the hands-free shortcut (default **Ctrl + Windows + Space**) and **Escape** cancellation.
- **Tap once while locked:** stop locked mode and restore Discord's prior voice state.
- Optionally play a Discord Soundboard announcement when dictation starts; mute and playback requests are sent back-to-back. Paste the sound ID in settings (for example, `1328911757753712702`).
- Optionally self-deafen Discord too, blocking incoming audio while dictating. When announcing, deafening follows a short 100 ms grace period after Discord accepts the sound request, allowing the clip to start instead of waiting for the whole sound to finish.
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

The Rust backend observes the same physical keyboard shortcuts as your selected dictation app using `WH_KEYBOARD_LL`, mirrors its hold/double-tap/hands-free gestures, and writes Discord voice settings through Discord desktop's local named pipe. It does not read speech or typed text.

The tray process starts without a webview. Tauri creates the settings webview only when requested and destroys it when closed, keeping normal background usage small.

## Requirements

- Windows 10/11 x64
- Willow Voice or Wispr Flow for Windows
- Discord desktop—not Discord in a browser
- Microsoft Edge WebView2 Runtime for the settings window (normally included with Windows)

## Wispr Flow setup

1. In bridge settings, choose **Wispr Flow** under **Dictation app**. Existing installations stay on Willow until you switch.
2. Match **Dictation shortcut** to Flow's push-to-talk shortcut (default **Ctrl + Windows**) and **Hands-free shortcut** to Flow's hands-free shortcut (default **Ctrl + Windows + Space**). Capture them separately if customized.
3. Leave **Shortcut mode** on **Auto**: hold to dictate, double-tap within half a second to lock, or use the hands-free shortcut. Press the shortcut again to stop; **Escape** cancels and restores Discord.
4. Soundboard announcements, optional deafening, and prior-state restoration work with either app. Only the selected profile is monitored; do not run both apps on the same shortcut at the same time.

The bridge mirrors keyboard gestures, not Flow's actual recording state. Clicking the Flow Bar to start/stop, automatic stops, mouse shortcuts, or a customized cancel shortcut are not detected. Use the matching keyboard shortcuts to keep Discord synchronized. Flow may also reject a shortcut when setup is incomplete or dictation is unavailable.

## RØDECaster MIDI pads

The bridge can also trigger Discord's built-in Soundboard from MIDI pads. Create
`%APPDATA%\com.willowdiscordbridge.desktop\midi-pads.json` and restart the bridge:

```json
{
  "device": "MIDI function",
  "channel": 1,
  "pads": [
    { "control": 18, "name": "My sound", "sound_id": "YOUR_SOUND_ID" }
  ]
}
```

Use a numeric Discord sound ID. Configure each RØDECaster pad as **MIDI →
Momentary** and match its displayed CC control number and channel. Positive CC
values trigger playback; release values of zero are ignored. On the Duo, default
Bank 4 controls run down the left column (18–20), then down the right (21–23).
Leave pad-editing/Transfer Mode before testing the physical buttons.

The listener uses the existing Discord authorization and does not change voice
mute/deafen state. Discord must be running and you must be in an eligible voice
channel. Playback failures and MIDI connection status appear in `midi-pads.log`
beside the mapping file. The bridge waits for a disconnected MIDI device to return;
restart it if a rapid reconnect is not detected. Remove the mapping file and restart
to disable MIDI input. Configuration changes take effect after restarting.

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

- Synchronization is based on shared keyboard shortcuts, not actual recording-state events. Stops initiated through app UI or automatic cancellation can leave the bridge active; use the stop shortcut (or Escape with the Wispr profile) to restore Discord.
- Discord RPC requires Discord desktop and developer-application authorization. Soundboard playback uses Discord's undocumented local `GET_SOUNDBOARD_SOUNDS` and `PLAY_SOUNDBOARD_SOUND` RPC commands; the bridge looks up the sound's source server automatically and caches it for the connection to avoid repeating that lookup during dictation. Playing it in another server may require **Use External Sounds** permission. Errors appear in the settings connection panel without preventing self-mute. Mute and playback requests are pipelined, not atomic; there can be a brief open-mic window before Discord applies mute. A sound will not play if you were already deafened or if another sound was attempted in the last five seconds.
- If Discord's prior state cannot be read, the bridge fails closed rather than risk unmuting you.
- A hard process kill cannot perform graceful cleanup, although Discord normally reverts RPC-controlled voice settings when its controller disconnects.

## Attribution and license

This project contains work adapted from [Hush](https://github.com/MatthysDev/hush), created by [Matthys Ducrocq](https://github.com/MatthysDev). Hush established the core design of observing an existing dictation shortcut and controlling Discord through local RPC instead of injected keyboard events.

The upstream MIT notice is retained verbatim in `LICENSE`. See `NOTICE` for adaptation details and `UPSTREAM_README.md` for the preserved upstream documentation.
