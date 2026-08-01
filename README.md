# Willow Discord Bridge for Windows

A Windows notification-area app that self-mutes Discord while Willow Voice is listening.

> **Attribution:** This project is a Windows adaptation of the Discord RPC and
> shortcut-gesture work pioneered in [Hush](https://github.com/MatthysDev/hush)
> by [Matthys Ducrocq](https://github.com/MatthysDev). Hush's original MIT
> license and copyright notice are retained in `LICENSE`; see `NOTICE` for details.
>
> This is an unofficial community project and is not affiliated with or endorsed
> by Willow Voice or Discord.

## What it does

- **Hold Ctrl + Windows:** mute Discord until the shortcut is released.
- **Double-tap Ctrl + Windows:** keep Discord muted while Willow's locked/hands-free dictation is active.
- **Tap once while locked:** stop locked mode and restore Discord's previous voice state.
- Preserve a pre-existing Discord mute/deafen state instead of blindly unmuting.
- Reconnect when Discord desktop restarts.
- Optionally start with Windows.

The shortcut is configurable. It must match the shortcut configured in Willow Voice.

## How it works

Willow does not currently expose a Windows plug-in or recording-state API, and Discord Rich Presence cannot control voice. The bridge therefore runs as a small local tray process:

1. `uiohook-napi` observes only the configured global shortcut.
2. The gesture state machine mirrors Willow's hold/double-tap behavior.
3. Discord's local desktop RPC receives `GET_VOICE_SETTINGS` and `SET_VOICE_SETTINGS` commands.

It does not capture audio, inspect dictated text, inject keyboard events, or run an internet-facing server. Discord's IPC and Windows DPAPI credential encryption stay local to the PC.

## Requirements

- Windows 10/11 x64
- Willow Voice for Windows
- Discord desktop—not Discord in a browser

## Discord RPC setup

Discord requires each local RPC client to have a developer application identity:

1. Go to <https://discord.com/developers/applications> and select **New Application**.
2. Add your Discord account under the application's **App Testers** while it is unapproved.
3. In **OAuth2**, copy the **Client ID** and generate/copy a **Client Secret**.
4. Add this redirect URI exactly: `http://localhost`
5. Start Willow Discord Bridge and paste both values.
6. Select **Save and connect**, then approve Discord's authorization prompt.

The Client Secret and OAuth tokens are encrypted with Windows DPAPI before being saved. The Client ID is not secret.

## Develop

```powershell
npm install
npm test
npm run typecheck
npm start
```

Enable runtime logging with:

```powershell
$env:WILLOW_BRIDGE_DEBUG = "1"
npm start
```

Log file:

`%LOCALAPPDATA%\Willow Discord Bridge\bridge-debug.log`

## Build the installer

```powershell
npm run dist:win
```

Output:

`release\Willow-Discord-Bridge-0.2.0-Setup.exe`

The build is currently unsigned, so Windows SmartScreen may display a warning.

## CI and automatic releases

GitHub Actions tests and builds every push to `main` and every pull request using the native Go-based TypeScript 7 compiler. When `package.json` contains a version that does not yet have a matching GitHub release, a successful push to `main` automatically:

1. builds the Windows installer;
2. creates the corresponding `v<version>` Git tag at that commit;
3. creates a GitHub Release with generated release notes; and
4. uploads the installer to that release.

Create the next release with `npm version patch`, `npm version minor`, or `npm version major`, then push the commit to `main`. Pushing more commits without changing the version runs only the fast CI path and does not duplicate the release.

## Limits

- Willow has no public recording-state event, so synchronization is based on the shared physical shortcut and its gestures.
- Discord RPC requires the Discord desktop app and developer-app authorization.
- If the bridge cannot read Discord's prior mute state, it fails closed and does not automatically unmute.
- A hard process kill cannot run graceful cleanup, although Discord documents that RPC-controlled voice settings revert when the controlling RPC application disconnects.

## Attribution and license

This project contains work adapted from [Hush](https://github.com/MatthysDev/hush), created by [Matthys Ducrocq](https://github.com/MatthysDev). The original project established the core approach of monitoring a dictation shortcut and controlling Discord mute through local RPC rather than injected keyboard events.

The upstream MIT copyright and permission notice are retained verbatim in `LICENSE`. Additional attribution and a summary of the Windows adaptation are in `NOTICE`; the upstream README is preserved as `UPSTREAM_README.md`.
