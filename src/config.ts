import { BridgeConfig, DiscordRpc } from './types';

export const DEFAULT_CONFIG: BridgeConfig = {
  // `cmd` is the internal cross-platform name used by uiohook for the Windows key.
  shortcut: { mods: ['ctrl', 'cmd'], key: '' },
  discordRpc: { clientId: '', clientSecret: '' },
  mode: 'auto',
  unmuteDelayMs: 0,
  launchAtLogin: true,
};

export function validateConfig(config: BridgeConfig): void {
  if (!config || !config.shortcut) throw new Error('Config is missing the shortcut');
  if (config.shortcut.mods.length === 0 && !config.shortcut.key) {
    throw new Error('Shortcut must have at least one key or modifier');
  }
  const validMods = new Set(['ctrl', 'alt', 'cmd', 'shift']);
  if (config.shortcut.mods.some((mod) => !validMods.has(mod))) {
    throw new Error('Shortcut contains an unsupported modifier');
  }
  if (!['auto', 'hold', 'toggle'].includes(config.mode)) {
    throw new Error('Shortcut mode is invalid');
  }
  if (!Number.isFinite(config.unmuteDelayMs) || config.unmuteDelayMs < 0 || config.unmuteDelayMs > 5_000) {
    throw new Error('Unmute delay must be between 0 and 5000 milliseconds');
  }
}

export function getConfig(): BridgeConfig {
  validateConfig(DEFAULT_CONFIG);
  return DEFAULT_CONFIG;
}

export function preserveDiscordTokens(previous: DiscordRpc, next: DiscordRpc): DiscordRpc {
  const sameCredentials =
    previous.clientId === next.clientId && previous.clientSecret === next.clientSecret;
  if (!sameCredentials) return { clientId: next.clientId, clientSecret: next.clientSecret };
  return {
    ...next,
    accessToken: previous.accessToken,
    refreshToken: previous.refreshToken,
    tokenExpiresAt: previous.tokenExpiresAt,
  };
}
