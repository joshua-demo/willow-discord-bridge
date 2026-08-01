'use strict';

const api = window.willowBridge;
const MOD_ORDER = ['ctrl', 'alt', 'cmd', 'shift'];
const MOD_LABEL = { ctrl: 'Ctrl', alt: 'Alt', cmd: 'Win', shift: 'Shift' };
const $ = (id) => document.getElementById(id);

const elements = {
  statusDot: $('status-dot'),
  statusLabel: $('status-label'),
  rpcState: $('rpc-state'),
  rpcError: $('rpc-error'),
  rpcId: $('rpc-id'),
  rpcSecret: $('rpc-secret'),
  connect: $('connect'),
  setupHelp: $('setup-help'),
  setupSteps: $('setup-steps'),
  openPortal: $('open-portal'),
  capture: $('capture-shortcut'),
  modes: $('mode-segment'),
  delay: $('delay'),
  delayValue: $('delay-value'),
  launchAtLogin: $('launch-at-login'),
  save: $('save'),
  quit: $('quit'),
  error: $('error'),
  version: $('version'),
};

let config;
let capturing = false;

function comboLabel(combo) {
  if (!combo) return 'Not set';
  const parts = MOD_ORDER.filter((mod) => combo.mods.includes(mod)).map((mod) => MOD_LABEL[mod]);
  if (combo.key) parts.push(combo.key.toUpperCase());
  return parts.join(' + ') || 'Not set';
}

function showError(message = '') {
  elements.error.textContent = message;
}

function syncInputsToConfig() {
  config.discordRpc = {
    ...config.discordRpc,
    clientId: elements.rpcId.value.trim(),
    clientSecret: elements.rpcSecret.value.trim(),
  };
  config.unmuteDelayMs = Number(elements.delay.value);
  config.launchAtLogin = elements.launchAtLogin.checked;
}

function render() {
  elements.rpcId.value = config.discordRpc.clientId || '';
  elements.rpcSecret.value = config.discordRpc.clientSecret || '';
  elements.capture.textContent = comboLabel(config.shortcut);
  elements.delay.value = String(config.unmuteDelayMs);
  elements.delayValue.textContent = String(config.unmuteDelayMs);
  elements.launchAtLogin.checked = config.launchAtLogin;
  for (const button of elements.modes.querySelectorAll('button')) {
    button.classList.toggle('active', button.dataset.mode === config.mode);
  }
}

async function save() {
  syncInputsToConfig();
  showError();
  const result = await api.saveConfig(config);
  if (!result.ok) {
    showError(result.error || 'Could not save settings.');
    return false;
  }
  config = result.config;
  render();
  return true;
}

function renderStatus(status) {
  if (!status.engineReady) {
    elements.statusLabel.textContent = 'Shortcut listener unavailable';
    elements.statusDot.className = 'dot warn';
  } else if (status.active) {
    elements.statusLabel.textContent = 'Discord muted — Willow is listening';
    elements.statusDot.className = 'dot active';
  } else {
    elements.statusLabel.textContent = 'Ready';
    elements.statusDot.className = 'dot idle';
  }

  const labels = {
    connected: 'Connected',
    connecting: 'Connecting…',
    disconnected: 'Not connected',
  };
  elements.rpcState.textContent = labels[status.rpc] || 'Not connected';
  elements.rpcState.className = `pill ${status.rpc === 'connected' ? 'pill-on' : status.rpc === 'connecting' ? 'pill-warn' : 'pill-off'}`;
  elements.rpcError.textContent = status.rpcError || '';
  elements.rpcError.hidden = !status.rpcError;
}

elements.capture.addEventListener('click', async () => {
  if (capturing) return;
  capturing = true;
  showError();
  elements.capture.classList.add('armed');
  elements.capture.textContent = 'Press shortcut…';
  const result = await api.captureCombo();
  elements.capture.classList.remove('armed');
  capturing = false;

  if (result.combo) {
    config.shortcut = result.combo;
    await save();
  } else if (result.reason === 'unsupported') {
    showError('That key is not supported. Use modifiers alone, or modifiers plus A–Z, 0–9, or F1–F24.');
  } else if (result.reason === 'timeout') {
    showError('No shortcut was detected. Try again and release the keys within eight seconds.');
  }
  render();
});

elements.modes.addEventListener('click', (event) => {
  const mode = event.target.dataset.mode;
  if (!mode) return;
  config.mode = mode;
  render();
});

elements.delay.addEventListener('input', () => {
  elements.delayValue.textContent = elements.delay.value;
});

elements.save.addEventListener('click', async () => {
  if (await save()) {
    elements.save.textContent = 'Saved';
    setTimeout(() => { elements.save.textContent = 'Save settings'; }, 1_200);
  }
});

elements.connect.addEventListener('click', async () => {
  if (!await save()) return;
  elements.connect.textContent = 'Connecting…';
  await api.reconnectDiscord();
  elements.connect.textContent = 'Save and connect';
});

elements.setupHelp.addEventListener('click', () => {
  elements.setupSteps.hidden = !elements.setupSteps.hidden;
});

elements.openPortal.addEventListener('click', () => {
  api.openExternal('https://discord.com/developers/applications');
});

elements.quit.addEventListener('click', () => api.quit());
api.onStatus(renderStatus);

(async () => {
  config = await api.getConfig();
  render();
  elements.version.textContent = `Version ${await api.getVersion()}`;
})();
