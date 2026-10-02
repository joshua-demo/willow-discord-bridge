type Mode = 'auto' | 'hold' | 'toggle';
type DictationApp = 'willow' | 'wispr';
type Mod = 'ctrl' | 'alt' | 'cmd' | 'shift';
type Shortcut = { mods: Mod[]; key: string };
type DiscordConfig = { clientId: string; clientSecret: string };
type Config = {
  dictationApp: DictationApp;
  handsFreeShortcut: Shortcut;
  shortcut: Shortcut;
  discordRpc: DiscordConfig;
  mode: Mode;
  deafenWhileActive: boolean;
  soundboardSoundId: string;
  unmuteDelayMs: number;
  launchAtLogin: boolean;
};
type Status = {
  active: boolean;
  engineReady: boolean;
  rpc: 'disconnected' | 'connecting' | 'connected';
  rpcError: string | null;
};
type SaveResult = { ok: true; config: Config } | { ok: false; error: string };
type CaptureResult = { combo: Shortcut | null; reason?: string };

declare global {
  interface Window {
    __TAURI__: {
      core: { invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> };
      event: { listen<T>(event: string, callback: (event: { payload: T }) => void): Promise<() => void> };
    };
  }
}

const invoke = window.__TAURI__.core.invoke;
const MOD_ORDER: Mod[] = ['ctrl', 'alt', 'cmd', 'shift'];
const MOD_LABEL: Record<Mod, string> = { ctrl: 'Ctrl', alt: 'Alt', cmd: 'Win', shift: 'Shift' };
const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const elements = {
  statusDot: $('status-dot'),
  statusLabel: $('status-label'),
  rpcState: $('rpc-state'),
  rpcError: $('rpc-error'),
  rpcId: $<HTMLInputElement>('rpc-id'),
  rpcSecret: $<HTMLInputElement>('rpc-secret'),
  connect: $<HTMLButtonElement>('connect'),
  setupHelp: $<HTMLButtonElement>('setup-help'),
  setupSteps: $('setup-steps'),
  openPortal: $<HTMLButtonElement>('open-portal'),
  capture: $<HTMLButtonElement>('capture-shortcut'),
  captureHandsFree: $<HTMLButtonElement>('capture-hands-free'),
  apps: $('app-segment'),
  wisprShortcuts: $('wispr-shortcuts'),
  shortcutHelp: $('shortcut-help'),
  modeHelp: $('mode-help'),
  modes: $('mode-segment'),
  delay: $<HTMLInputElement>('delay'),
  delayValue: $('delay-value'),
  deafenWhileActive: $<HTMLInputElement>('deafen-while-active'),
  soundboardSoundId: $<HTMLInputElement>('soundboard-sound-id'),
  launchAtLogin: $<HTMLInputElement>('launch-at-login'),
  error: $('error'),
  version: $('version'),
};

let config: Config;
let capturing = false;
let saveRevision = 0;
let saveQueue = Promise.resolve();

function comboLabel(combo: Shortcut): string {
  const parts = MOD_ORDER.filter((mod) => combo.mods.includes(mod)).map((mod) => MOD_LABEL[mod]);
  if (combo.key) parts.push(combo.key === 'VK_20' ? 'Space' : combo.key.toUpperCase());
  return parts.join(' + ') || 'Not set';
}

function showError(message = ''): void {
  elements.error.textContent = message;
}

function syncInputs(): void {
  config.discordRpc = {
    clientId: elements.rpcId.value.trim(),
    clientSecret: elements.rpcSecret.value.trim(),
  };
  config.unmuteDelayMs = Number(elements.delay.value);
  config.deafenWhileActive = elements.deafenWhileActive.checked;
  config.soundboardSoundId = elements.soundboardSoundId.value.trim();
  config.launchAtLogin = elements.launchAtLogin.checked;
}

function render(): void {
  elements.rpcId.value = config.discordRpc.clientId || '';
  elements.rpcSecret.value = config.discordRpc.clientSecret || '';
  elements.capture.textContent = comboLabel(config.shortcut);
  elements.captureHandsFree.textContent = comboLabel(config.handsFreeShortcut);
  elements.wisprShortcuts.hidden = config.dictationApp !== 'wispr';
  const appName = config.dictationApp === 'wispr' ? 'Wispr Flow' : 'Willow Voice';
  elements.shortcutHelp.textContent = `Match the push-to-talk shortcut in ${appName}. The default is Ctrl + Windows.`;
  elements.modeHelp.textContent = config.dictationApp === 'wispr'
    ? 'Auto: hold to dictate or double-tap for hands-free. The hands-free shortcut also starts/stops dictation.'
    : 'Auto mirrors Willow: hold to dictate, double-tap to lock, tap again to stop.';
  for (const button of elements.apps.querySelectorAll<HTMLButtonElement>('button')) {
    button.classList.toggle('active', button.dataset.app === config.dictationApp);
  }
  elements.delay.value = String(config.unmuteDelayMs);
  elements.delayValue.textContent = String(config.unmuteDelayMs);
  elements.deafenWhileActive.checked = config.deafenWhileActive;
  elements.soundboardSoundId.value = config.soundboardSoundId;
  elements.launchAtLogin.checked = config.launchAtLogin;
  for (const button of elements.modes.querySelectorAll<HTMLButtonElement>('button')) {
    button.classList.toggle('active', button.dataset.mode === config.mode);
  }
}

function save(): Promise<boolean> {
  syncInputs();
  showError();
  const snapshot = structuredClone(config);
  const revision = ++saveRevision;
  const task = saveQueue.then(async () => {
    const result = await invoke<SaveResult>('save_config', { config: snapshot });
    if (!result.ok) {
      if (revision === saveRevision) showError(result.error);
      return false;
    }
    if (revision === saveRevision) {
      config = result.config;
      render();
    }
    return true;
  });
  saveQueue = task.then(() => undefined, () => undefined);
  return task;
}

function renderStatus(status: Status): void {
  if (!status.engineReady) {
    elements.statusLabel.textContent = 'Shortcut listener unavailable';
    elements.statusDot.className = 'dot warn';
  } else if (status.active) {
    const appName = config.dictationApp === 'wispr' ? 'Wispr Flow' : 'Willow';
    elements.statusLabel.textContent = config.deafenWhileActive
      ? `Discord muted/deafening — ${appName} is listening`
      : `Discord muted — ${appName} is listening`;
    elements.statusDot.className = 'dot active';
  } else {
    elements.statusLabel.textContent = 'Ready';
    elements.statusDot.className = 'dot idle';
  }
  const labels = { connected: 'Connected', connecting: 'Connecting…', disconnected: 'Not connected' };
  elements.rpcState.textContent = labels[status.rpc];
  elements.rpcState.className = `pill ${status.rpc === 'connected' ? 'pill-on' : status.rpc === 'connecting' ? 'pill-warn' : 'pill-off'}`;
  elements.rpcError.textContent = status.rpcError || '';
  elements.rpcError.hidden = !status.rpcError;
}

async function captureShortcut(field: 'shortcut' | 'handsFreeShortcut', button: HTMLButtonElement): Promise<void> {
  if (capturing) return;
  capturing = true;
  showError();
  button.classList.add('armed');
  button.textContent = 'Press shortcut…';
  try {
    const result = await invoke<CaptureResult>('capture_shortcut');
    if (result.combo) {
      config[field] = result.combo;
      await save();
    } else if (result.reason === 'timeout') {
      showError('No shortcut was detected within eight seconds.');
    } else if (result.reason && result.reason !== 'cancelled') {
      showError(result.reason);
    }
  } catch (error) {
    showError(String(error));
  } finally {
    button.classList.remove('armed');
    capturing = false;
    render();
  }
}

elements.capture.addEventListener('click', () => { void captureShortcut('shortcut', elements.capture); });
elements.captureHandsFree.addEventListener('click', () => { void captureShortcut('handsFreeShortcut', elements.captureHandsFree); });
elements.apps.addEventListener('click', (event) => {
  const target = event.target as HTMLButtonElement;
  if (!target.dataset.app || capturing) return;
  config.dictationApp = target.dataset.app as DictationApp;
  config.mode = 'auto';
  render();
  void save();
});

elements.modes.addEventListener('click', (event) => {
  const target = event.target as HTMLButtonElement;
  if (!target.dataset.mode) return;
  config.mode = target.dataset.mode as Mode;
  render();
  void save();
});

elements.delay.addEventListener('input', () => { elements.delayValue.textContent = elements.delay.value; });
elements.delay.addEventListener('change', () => { void save(); });
elements.deafenWhileActive.addEventListener('change', () => { void save(); });
elements.soundboardSoundId.addEventListener('change', () => { void save(); });
elements.launchAtLogin.addEventListener('change', () => { void save(); });
elements.connect.addEventListener('click', async () => {
  if (!await save()) return;
  elements.connect.textContent = 'Connecting…';
  await invoke('reconnect_discord');
  elements.connect.textContent = 'Save and connect';
});
elements.setupHelp.addEventListener('click', () => { elements.setupSteps.hidden = !elements.setupSteps.hidden; });
elements.openPortal.addEventListener('click', () => invoke('open_url', { url: 'https://discord.com/developers/applications' }));

(async () => {
  config = await invoke<Config>('get_config');
  render();
  elements.version.textContent = `Version ${await invoke<string>('get_version')}`;
  renderStatus(await invoke<Status>('get_status'));
  await window.__TAURI__.event.listen<Status>('bridge-status', (event) => renderStatus(event.payload));
  setInterval(async () => renderStatus(await invoke<Status>('get_status')), 2000);
})();

export {};
