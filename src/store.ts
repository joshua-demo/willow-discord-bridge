import { safeStorage } from 'electron';
import Store from 'electron-store';
import { DEFAULT_CONFIG, validateConfig } from './config';
import { BridgeConfig, Combo, DiscordRpc } from './types';

const store = new Store<{ config: unknown }>({
  name: 'willow-discord-bridge-config',
  defaults: { config: DEFAULT_CONFIG },
});

const ENCRYPTED_PREFIX = 'dpapi:';

function protect(value: string | undefined): string | undefined {
  if (!value || !safeStorage.isEncryptionAvailable()) return value;
  return ENCRYPTED_PREFIX + safeStorage.encryptString(value).toString('base64');
}

function unprotect(value: unknown): string | undefined {
  if (typeof value !== 'string' || !value) return undefined;
  if (!value.startsWith(ENCRYPTED_PREFIX)) return value; // migrate an older plaintext config
  try {
    return safeStorage.decryptString(Buffer.from(value.slice(ENCRYPTED_PREFIX.length), 'base64'));
  } catch {
    return undefined;
  }
}

function decodeDiscord(raw: unknown): DiscordRpc {
  const discord = (raw && typeof raw === 'object' ? raw : {}) as Record<string, unknown>;
  return {
    clientId: typeof discord.clientId === 'string' ? discord.clientId : '',
    clientSecret: unprotect(discord.clientSecret) ?? '',
    accessToken: unprotect(discord.accessToken),
    refreshToken: unprotect(discord.refreshToken),
    tokenExpiresAt: typeof discord.tokenExpiresAt === 'number' ? discord.tokenExpiresAt : undefined,
  };
}

function encodeConfig(config: BridgeConfig): Record<string, unknown> {
  return {
    ...config,
    discordRpc: {
      clientId: config.discordRpc.clientId,
      clientSecret: protect(config.discordRpc.clientSecret),
      accessToken: protect(config.discordRpc.accessToken),
      refreshToken: protect(config.discordRpc.refreshToken),
      tokenExpiresAt: config.discordRpc.tokenExpiresAt,
    },
  };
}

function migrate(raw: Record<string, unknown>): BridgeConfig {
  const shortcut = (raw.shortcut ?? raw.trigger ?? DEFAULT_CONFIG.shortcut) as Combo;
  const mode = raw.mode === 'handsfree' ? 'auto' : raw.mode;
  return {
    shortcut,
    discordRpc: decodeDiscord(raw.discordRpc),
    mode: (mode as BridgeConfig['mode']) ?? DEFAULT_CONFIG.mode,
    unmuteDelayMs: (raw.unmuteDelayMs as number) ?? DEFAULT_CONFIG.unmuteDelayMs,
    launchAtLogin: (raw.launchAtLogin as boolean) ?? DEFAULT_CONFIG.launchAtLogin,
  };
}

export function loadConfig(): BridgeConfig {
  const config = migrate(store.get('config') as Record<string, unknown>);
  try {
    validateConfig(config);
    // Re-save once so plaintext credentials from prototype builds move into DPAPI.
    store.set('config', encodeConfig(config));
    return config;
  } catch {
    const safe = {
      ...DEFAULT_CONFIG,
      discordRpc: config.discordRpc ?? DEFAULT_CONFIG.discordRpc,
    };
    store.set('config', encodeConfig(safe));
    return safe;
  }
}

export function saveConfig(config: BridgeConfig): BridgeConfig {
  validateConfig(config);
  store.set('config', encodeConfig(config));
  return config;
}
