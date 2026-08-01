import { describe, expect, it } from 'vitest';
import { DEFAULT_CONFIG, getConfig, preserveDiscordTokens, validateConfig } from '../src/config';

describe('Windows defaults', () => {
  it('uses Ctrl+Windows and auto gesture mode', () => {
    const config = getConfig();
    expect(config.shortcut).toEqual({ mods: ['ctrl', 'cmd'], key: '' });
    expect(config.mode).toBe('auto');
    expect(config.unmuteDelayMs).toBe(0);
    expect(config.launchAtLogin).toBe(true);
  });

  it('starts without Discord credentials', () => {
    expect(DEFAULT_CONFIG.discordRpc).toEqual({ clientId: '', clientSecret: '' });
  });
});

describe('validateConfig', () => {
  it('accepts a modifier-only shortcut', () => {
    expect(() => validateConfig({
      ...DEFAULT_CONFIG,
      shortcut: { mods: ['ctrl', 'cmd'], key: '' },
    })).not.toThrow();
  });

  it('accepts a key and modifier shortcut', () => {
    expect(() => validateConfig({
      ...DEFAULT_CONFIG,
      shortcut: { mods: ['ctrl'], key: 'F13' },
    })).not.toThrow();
  });

  it('rejects an empty shortcut', () => {
    expect(() => validateConfig({
      ...DEFAULT_CONFIG,
      shortcut: { mods: [], key: '' },
    })).toThrow(/shortcut/i);
  });

  it('rejects unsupported modifiers and modes', () => {
    expect(() => validateConfig({
      ...DEFAULT_CONFIG,
      shortcut: { mods: ['super' as never], key: '' },
    })).toThrow(/modifier/i);
    expect(() => validateConfig({
      ...DEFAULT_CONFIG,
      mode: 'invalid' as never,
    })).toThrow(/mode/i);
  });

  it('rejects unreasonable unmute delays', () => {
    expect(() => validateConfig({ ...DEFAULT_CONFIG, unmuteDelayMs: -1 })).toThrow(/delay/i);
    expect(() => validateConfig({ ...DEFAULT_CONFIG, unmuteDelayMs: 5_001 })).toThrow(/delay/i);
  });
});

describe('preserveDiscordTokens', () => {
  const previous = {
    clientId: 'id',
    clientSecret: 'secret',
    accessToken: 'access',
    refreshToken: 'refresh',
    tokenExpiresAt: 123,
  };

  it('preserves main-process tokens when credentials are unchanged', () => {
    expect(preserveDiscordTokens(previous, { clientId: 'id', clientSecret: 'secret' })).toEqual(previous);
  });

  it('drops tokens when either credential changes', () => {
    expect(preserveDiscordTokens(previous, { clientId: 'new', clientSecret: 'secret' }))
      .toEqual({ clientId: 'new', clientSecret: 'secret' });
    expect(preserveDiscordTokens(previous, { clientId: 'id', clientSecret: 'new' }))
      .toEqual({ clientId: 'id', clientSecret: 'new' });
  });
});
