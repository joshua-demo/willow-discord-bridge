import { app, BrowserWindow, ipcMain, Menu, nativeImage, shell, Tray } from 'electron';
import * as path from 'path';
import { uIOhook } from 'uiohook-napi';
import { BRAND } from './brand';
import { comboLabel } from './combo';
import { DEFAULT_CONFIG, preserveDiscordTokens } from './config';
import { dbg, LOG_FILE } from './debug';
import { DiscordRpcMuter } from './discord-mute';
import {
  isUiohookEscape,
  UiohookInputEngine,
  uiohookKeyToKey,
  uiohookModifierOf,
} from './input-engine';
import { Orchestrator } from './orchestrator';
import { loadConfig, saveConfig } from './store';
import { BridgeConfig, Combo, InputEngine, Mod } from './types';

interface RawKeyEvent {
  keycode: number;
}

export type CaptureResult =
  | { combo: Combo }
  | { combo: null; reason: 'cancelled' | 'unsupported' | 'timeout' | 'busy' };

const ASSETS = path.join(__dirname, '..', 'assets');
const RENDERER = path.join(__dirname, '..', 'renderer', 'index.html');
const RPC_RETRY_MS = 15_000;

let tray: Tray | null = null;
let win: BrowserWindow | null = null;
let cfg: BridgeConfig = DEFAULT_CONFIG;
let input: InputEngine | null = null;
let orchestrator: Orchestrator | null = null;
let active = false;
let engineReady = false;
let capturing = false;
let retryTimer: ReturnType<typeof setTimeout> | null = null;
let connectInFlight: Promise<void> | null = null;
let quitting = false;

const discord = new DiscordRpcMuter();

function trayImage(activeState: boolean) {
  // Windows does not support macOS template images. Use the full-color ICO so
  // the tray icon remains visible in both light and dark taskbar themes.
  const icon = nativeImage.createFromPath(path.join(ASSETS, 'icon.ico'));
  return activeState ? icon.resize({ width: 20, height: 20 }) : icon.resize({ width: 16, height: 16 });
}

function rpcLabel(): string {
  const state = discord.getState();
  if (state === 'connected') return 'Connected';
  if (state === 'connecting') return 'Connecting…';
  return 'Not connected';
}

function statusText(): string {
  if (!engineReady) return 'Shortcut listener unavailable';
  if (active) return 'Discord muted — Willow is listening';
  return 'Ready';
}

function pushStatus(): void {
  const status = {
    active,
    engineReady,
    rpc: discord.getState(),
    rpcError: discord.getError(),
  };
  win?.webContents.send('status', status);
  if (!tray) return;
  tray.setImage(trayImage(active));
  tray.setToolTip(`${BRAND.name} — ${statusText()}`);
  tray.setContextMenu(Menu.buildFromTemplate([
    { label: `${BRAND.name} — ${statusText()}`, enabled: false },
    { type: 'separator' },
    { label: `Willow shortcut: ${comboLabel(cfg.shortcut)}`, enabled: false },
    { label: `Discord: ${rpcLabel()}`, enabled: false },
    { type: 'separator' },
    { label: 'Settings…', click: showWindow },
    { label: 'Reconnect Discord', click: () => void connectDiscord() },
    { type: 'separator' },
    { label: 'Quit', click: () => app.quit() },
  ]));
}

function publicConfig(config: BridgeConfig): BridgeConfig {
  return {
    ...config,
    discordRpc: {
      clientId: config.discordRpc.clientId,
      clientSecret: config.discordRpc.clientSecret,
    },
  };
}

function applyLaunchAtLogin(next: BridgeConfig): void {
  try {
    app.setLoginItemSettings({
      openAtLogin: next.launchAtLogin,
      args: ['--hidden'],
    });
  } catch (error) {
    dbg('launch-at-login failed', error instanceof Error ? error.message : String(error));
  }
}

async function releaseAndStopInput(): Promise<void> {
  input?.stop();
  input = null;
  await orchestrator?.forceRelease();
  orchestrator = null;
  active = false;
}

async function applyConfig(next: BridgeConfig): Promise<void> {
  await releaseAndStopInput();
  cfg = next;

  orchestrator = new Orchestrator(discord, cfg, (isActive) => {
    active = isActive;
    pushStatus();
  });
  input = new UiohookInputEngine(cfg.shortcut);
  input.onPress(() => {
    if (!capturing) void orchestrator?.onPress();
  });
  input.onRelease(() => {
    if (!capturing) void orchestrator?.onRelease();
  });

  try {
    input.start();
    engineReady = true;
  } catch (error) {
    engineReady = false;
    dbg('shortcut listener failed', error instanceof Error ? error.message : String(error));
  }
  dbg('engine started', { shortcut: comboLabel(cfg.shortcut), mode: cfg.mode, engineReady, logFile: LOG_FILE });
  pushStatus();
}

function connectDiscord(): Promise<void> {
  if (connectInFlight) return connectInFlight;
  connectInFlight = doConnectDiscord().finally(() => {
    connectInFlight = null;
  });
  return connectInFlight;
}

async function doConnectDiscord(): Promise<void> {
  if (retryTimer) {
    clearTimeout(retryTimer);
    retryTimer = null;
  }
  const { clientId, clientSecret } = cfg.discordRpc;
  if (!clientId || !clientSecret) {
    await discord.disconnect();
    pushStatus();
    return;
  }

  const pending = discord.connect(clientId, clientSecret, {
    accessToken: cfg.discordRpc.accessToken,
    refreshToken: cfg.discordRpc.refreshToken,
    tokenExpiresAt: cfg.discordRpc.tokenExpiresAt,
  });
  pushStatus();
  const connected = await pending;

  const tokens = discord.getTokens();
  if (tokens) {
    cfg = {
      ...cfg,
      discordRpc: {
        ...cfg.discordRpc,
        accessToken: tokens.accessToken,
        refreshToken: tokens.refreshToken,
        tokenExpiresAt: tokens.tokenExpiresAt,
      },
    };
    try { saveConfig(cfg); } catch { /* credentials remain usable for this session */ }
  }

  if (!connected && !quitting) {
    retryTimer = setTimeout(() => void connectDiscord(), RPC_RETRY_MS);
  }
  pushStatus();
}

function wasAutoLaunched(): boolean {
  return process.argv.includes('--hidden');
}

function showWindow(): void {
  if (win) {
    win.show();
    win.focus();
    return;
  }
  win = new BrowserWindow({
    width: 600,
    height: 760,
    minWidth: 520,
    minHeight: 620,
    title: BRAND.name,
    backgroundColor: BRAND.colors.bg,
    show: false,
    autoHideMenuBar: true,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  void win.loadFile(RENDERER);
  win.once('ready-to-show', () => {
    win?.show();
    pushStatus();
  });
  win.on('closed', () => { win = null; });
}

function captureCombo(): Promise<CaptureResult> {
  return new Promise((resolve) => {
    if (capturing) return resolve({ combo: null, reason: 'busy' });
    capturing = true;
    let settled = false;
    const pressed = new Set<Mod>();
    let peak: Mod[] = [];

    const finish = (result: CaptureResult) => {
      if (settled) return;
      settled = true;
      capturing = false;
      uIOhook.off('keydown', onDown);
      uIOhook.off('keyup', onUp);
      resolve(result);
    };
    const onDown = (event: RawKeyEvent) => {
      const modifier = uiohookModifierOf(event.keycode);
      if (modifier) {
        pressed.add(modifier);
        if (pressed.size > peak.length) peak = [...pressed];
        return;
      }
      if (isUiohookEscape(event.keycode)) return finish({ combo: null, reason: 'cancelled' });
      const key = uiohookKeyToKey(event.keycode);
      if (!key) return finish({ combo: null, reason: 'unsupported' });
      finish({ combo: { mods: [...pressed], key } });
    };
    const onUp = (event: RawKeyEvent) => {
      if (uiohookModifierOf(event.keycode) && peak.length > 0) {
        finish({ combo: { mods: peak, key: '' } });
      }
    };

    uIOhook.on('keydown', onDown);
    uIOhook.on('keyup', onUp);
    setTimeout(() => finish({ combo: null, reason: 'timeout' }), 8_000);
  });
}

async function shutdown(): Promise<void> {
  if (retryTimer) clearTimeout(retryTimer);
  retryTimer = null;
  await releaseAndStopInput();
  await discord.disconnect();
}

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', showWindow);

  app.whenReady().then(async () => {
    // safeStorage (Windows DPAPI) is only available after Electron is ready.
    cfg = loadConfig();
    tray = new Tray(trayImage(false));
    tray.on('click', showWindow);

    discord.setOnDrop(() => {
      if (!quitting) retryTimer = setTimeout(() => void connectDiscord(), 3_000);
      pushStatus();
    });

    await applyConfig(cfg);
    applyLaunchAtLogin(cfg);
    void connectDiscord();
    if (!wasAutoLaunched()) showWindow();

    ipcMain.handle('config:get', () => publicConfig(cfg));
    ipcMain.handle('app:version', () => app.getVersion());
    ipcMain.handle('brand:get', () => ({ name: BRAND.name, tagline: BRAND.tagline }));
    ipcMain.handle('capture:combo', () => captureCombo());
    ipcMain.handle('config:set', async (_event, next: BridgeConfig) => {
      try {
        const previous = cfg;
        const merged: BridgeConfig = {
          ...next,
          discordRpc: preserveDiscordTokens(previous.discordRpc, next.discordRpc),
        };
        const saved = saveConfig(merged);
        await applyConfig(saved);
        if (saved.launchAtLogin !== previous.launchAtLogin) applyLaunchAtLogin(saved);
        const credentialsChanged =
          saved.discordRpc.clientId !== previous.discordRpc.clientId ||
          saved.discordRpc.clientSecret !== previous.discordRpc.clientSecret;
        if (credentialsChanged && connectInFlight) {
          // Let an in-flight OAuth exchange finish, then connect once with the
          // newly saved credentials instead of coalescing onto the stale attempt.
          void connectInFlight.finally(() => void connectDiscord());
        } else if (credentialsChanged || !discord.isConnected()) {
          void connectDiscord();
        }
        return { ok: true, config: publicConfig(saved) };
      } catch (error) {
        return { ok: false, error: error instanceof Error ? error.message : String(error) };
      }
    });
    ipcMain.handle('rpc:reconnect', async () => {
      await connectDiscord();
      return { state: discord.getState(), error: discord.getError() };
    });
    ipcMain.on('app:quit', () => app.quit());
    ipcMain.on('app:open-external', (_event, url: unknown) => {
      if (typeof url === 'string' && /^https:\/\//.test(url)) void shell.openExternal(url);
    });
  });

  app.on('before-quit', (event) => {
    if (quitting) return;
    event.preventDefault();
    quitting = true;
    void shutdown().finally(() => app.exit(0));
  });
  app.on('window-all-closed', () => { /* remain available in the Windows tray */ });
  process.on('SIGINT', () => app.quit());
  process.on('uncaughtException', (error) => {
    dbg('uncaught exception', error instanceof Error ? error.stack ?? error.message : String(error));
    app.quit();
  });
}
