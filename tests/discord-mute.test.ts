import { describe, it, expect, vi } from 'vitest';
import { DiscordRpcMuter } from '../src/discord-mute';

describe('DiscordRpcMuter best-effort behaviour', () => {
  it('starts disconnected', () => {
    const m = new DiscordRpcMuter();
    expect(m.isConnected()).toBe(false);
    expect(m.getState()).toBe('disconnected');
  });

  it('refuses to connect without credentials and records why', async () => {
    const m = new DiscordRpcMuter();
    const ok = await m.connect('', '');
    expect(ok).toBe(false);
    expect(m.getState()).toBe('disconnected');
    expect(m.getError()).toMatch(/required/);
  });

  it('setMute is a silent no-op when not connected (dictation must still proceed)', async () => {
    const m = new DiscordRpcMuter();
    await expect(m.setMute(true)).resolves.toBeUndefined();
    await expect(m.setMute(false)).resolves.toBeUndefined();
    expect(m.isConnected()).toBe(false);
  });

  it('disconnect is safe to call when never connected', async () => {
    const m = new DiscordRpcMuter();
    await expect(m.disconnect()).resolves.toBeUndefined();
    expect(m.getState()).toBe('disconnected');
  });
});

// --- FAKE RPC client for testing the new refresh-token / drop-watchdog flow ---
// Mirrors the subset of discord-rpc's real Client API our code depends on:
// connect(clientId), request(cmd, args), authenticate(accessToken),
// setVoiceSettings(settings), destroy(), and on(event, cb) — including firing
// a 'disconnected' event when the transport drops.

class FakeRpcClient {
  calls: { name: string; args: unknown[] }[] = [];
  handlers: Record<string, (...args: unknown[]) => void> = {};
  authorizeResponse: { code: string } = { code: 'fake-code' };
  destroyed = false;

  on(event: string, cb: (...args: unknown[]) => void): void {
    this.handlers[event] = cb;
  }

  fireDisconnected(): void {
    this.handlers['disconnected']?.();
  }

  async connect(clientId: string): Promise<unknown> {
    this.calls.push({ name: 'connect', args: [clientId] });
    return undefined;
  }

  async request(cmd: string, args: Record<string, unknown>): Promise<any> {
    this.calls.push({ name: 'request', args: [cmd, args] });
    if (cmd === 'AUTHORIZE') return this.authorizeResponse;
    return {};
  }

  async authenticate(accessToken: string): Promise<unknown> {
    this.calls.push({ name: 'authenticate', args: [accessToken] });
    return undefined;
  }

  async setVoiceSettings(settings: { mute?: boolean; deaf?: boolean }): Promise<unknown> {
    this.calls.push({ name: 'setVoiceSettings', args: [settings] });
    this.voiceSettings = { ...this.voiceSettings, ...settings };
    return undefined;
  }

  voiceSettings: { mute?: boolean; deaf?: boolean } = { mute: false, deaf: false };

  async getVoiceSettings(): Promise<{ mute?: boolean; deaf?: boolean }> {
    this.calls.push({ name: 'getVoiceSettings', args: [] });
    return this.voiceSettings;
  }

  async destroy(): Promise<void> {
    this.destroyed = true;
    this.calls.push({ name: 'destroy', args: [] });
  }
}

// Small helper for tests that need to control exactly when an async step
// resolves (e.g. to simulate an in-flight handshake or a hung token call).
function deferred<T>(): { promise: Promise<T>; resolve: (v: T) => void; reject: (e: unknown) => void } {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function makeFakeOauth(overrides: Partial<{
  isExpired: (expiresAt: number | undefined, nowMs: number) => boolean;
  refreshTokens: (...args: any[]) => Promise<any>;
  exchangeCode: (...args: any[]) => Promise<any>;
}> = {}) {
  return {
    isExpired: overrides.isExpired ?? vi.fn(() => true),
    refreshTokens: overrides.refreshTokens ?? vi.fn(async () => {
      throw new Error('refreshTokens should not be called');
    }),
    exchangeCode: overrides.exchangeCode ?? vi.fn(async () => {
      throw new Error('exchangeCode should not be called');
    }),
  };
}

describe('DiscordRpcMuter refresh-token login flow (fake RPC client)', () => {
  it('reuses a still-valid cached access token without any network call', async () => {
    let capturedClient: FakeRpcClient | null = null;
    const oauth = makeFakeOauth({ isExpired: vi.fn(() => false) });
    const m = new DiscordRpcMuter({
      createClient: () => {
        capturedClient = new FakeRpcClient();
        return capturedClient as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('fetchImpl should not be called');
      }) as any,
      now: () => 1000,
    });

    const ok = await m.connect('cid', 'secret', { accessToken: 'tok', tokenExpiresAt: 999999 });

    expect(ok).toBe(true);
    expect(m.getState()).toBe('connected');
    expect(oauth.refreshTokens).not.toHaveBeenCalled();
    expect(oauth.exchangeCode).not.toHaveBeenCalled();
    const client = capturedClient!;
    expect(client.calls.find((c) => c.name === 'request' && c.args[0] === 'AUTHORIZE')).toBeUndefined();
    expect(client.calls.some((c) => c.name === 'authenticate' && c.args[0] === 'tok')).toBe(true);
    expect(m.getTokens()?.accessToken).toBe('tok');
  });

  it('refreshes an expired access token via the refresh token', async () => {
    let capturedClient: FakeRpcClient | null = null;
    const oauth = makeFakeOauth({
      isExpired: vi.fn(() => true),
      refreshTokens: vi.fn(async () => ({ accessToken: 'new', refreshToken: 'r2', expiresAt: 999 })),
    });
    const m = new DiscordRpcMuter({
      createClient: () => {
        capturedClient = new FakeRpcClient();
        return capturedClient as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const ok = await m.connect('cid', 'secret', {
      accessToken: 'old',
      tokenExpiresAt: 1,
      refreshToken: 'r',
    });

    expect(ok).toBe(true);
    expect(oauth.refreshTokens).toHaveBeenCalledTimes(1);
    const client = capturedClient!;
    expect(client.calls.find((c) => c.name === 'request' && c.args[0] === 'AUTHORIZE')).toBeUndefined();
    expect(client.calls.some((c) => c.name === 'authenticate' && c.args[0] === 'new')).toBe(true);
    expect(m.getTokens()).toEqual({ accessToken: 'new', refreshToken: 'r2', tokenExpiresAt: 999 });
  });

  it('falls back to the AUTHORIZE popup flow when there are no tokens at all', async () => {
    let capturedClient: FakeRpcClient | null = null;
    const oauth = makeFakeOauth({
      exchangeCode: vi.fn(async (_fetch: unknown, opts: { code: string }) => ({
        accessToken: `exchanged-${opts.code}`,
        refreshToken: 'r3',
        expiresAt: 555,
      })),
    });
    const m = new DiscordRpcMuter({
      createClient: () => {
        capturedClient = new FakeRpcClient();
        return capturedClient as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const ok = await m.connect('cid', 'secret', {});

    expect(ok).toBe(true);
    const client = capturedClient!;
    expect(client.calls.some((c) => c.name === 'request' && c.args[0] === 'AUTHORIZE')).toBe(true);
    expect(oauth.exchangeCode).toHaveBeenCalledTimes(1);
    expect(client.calls.some((c) => c.name === 'authenticate' && c.args[0] === 'exchanged-fake-code')).toBe(true);
    expect(m.getTokens()?.accessToken).toBe('exchanged-fake-code');
  });

  it('falls back to AUTHORIZE when refreshTokens rejects', async () => {
    let capturedClient: FakeRpcClient | null = null;
    const oauth = makeFakeOauth({
      isExpired: vi.fn(() => true),
      refreshTokens: vi.fn(async () => {
        throw new Error('refresh token revoked');
      }),
      exchangeCode: vi.fn(async () => ({ accessToken: 'fromAuthorize', refreshToken: 'r4', expiresAt: 777 })),
    });
    const m = new DiscordRpcMuter({
      createClient: () => {
        capturedClient = new FakeRpcClient();
        return capturedClient as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const ok = await m.connect('cid', 'secret', { accessToken: 'old', tokenExpiresAt: 1, refreshToken: 'r' });

    expect(ok).toBe(true);
    expect(m.getState()).toBe('connected');
    const client = capturedClient!;
    expect(client.calls.some((c) => c.name === 'request' && c.args[0] === 'AUTHORIZE')).toBe(true);
    expect(client.calls.some((c) => c.name === 'authenticate' && c.args[0] === 'fromAuthorize')).toBe(true);
  });
});

describe('DiscordRpcMuter drop watchdog', () => {
  it('fires onDrop and flips to disconnected when the transport drops, but not after an intentional disconnect', async () => {
    let capturedClient: FakeRpcClient | null = null;
    const oauth = makeFakeOauth({ isExpired: vi.fn(() => false) });
    const m = new DiscordRpcMuter({
      createClient: () => {
        capturedClient = new FakeRpcClient();
        return capturedClient as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const onDrop = vi.fn();
    m.setOnDrop(onDrop);

    const ok = await m.connect('cid', 'secret', { accessToken: 'tok', tokenExpiresAt: 999999 });
    expect(ok).toBe(true);
    expect(m.getState()).toBe('connected');

    const client = capturedClient!;
    client.fireDisconnected();

    expect(onDrop).toHaveBeenCalledTimes(1);
    expect(m.getState()).toBe('disconnected');

    await m.disconnect();
    onDrop.mockClear();
    client.fireDisconnected();

    expect(onDrop).not.toHaveBeenCalled();
  });

  it('a socket close mid-handshake does not fire onDrop (session never reached connected)', async () => {
    let capturedClient: FakeRpcClient | null = null;
    const connectGate = deferred<unknown>();
    const oauth = makeFakeOauth({ isExpired: vi.fn(() => false) });
    const m = new DiscordRpcMuter({
      createClient: () => {
        capturedClient = new FakeRpcClient();
        capturedClient.connect = () => connectGate.promise;
        return capturedClient as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const onDrop = vi.fn();
    m.setOnDrop(onDrop);

    const connectPromise = m.connect('cid', 'secret', { accessToken: 'tok', tokenExpiresAt: 999999 });
    // Give connect() enough microtask ticks to create the client, register the
    // 'disconnected' handler, and start waiting on client.connect() (which we
    // control via connectGate) — i.e. we're mid-handshake, never 'connected'.
    await Promise.resolve();
    await Promise.resolve();
    expect(m.getState()).toBe('connecting');

    capturedClient!.fireDisconnected();

    expect(onDrop).not.toHaveBeenCalled();
    expect(m.getState()).toBe('connecting');

    // Let the handshake finish so the promise settles cleanly.
    connectGate.resolve(undefined);
    const ok = await connectPromise;
    expect(ok).toBe(true);
    expect(m.getState()).toBe('connected');
  });

  it('a stale connect() does not clobber a newer, successful connect()', async () => {
    const clients: FakeRpcClient[] = [];
    const refreshGate = deferred<{ accessToken: string; refreshToken: string; expiresAt: number }>();
    const call1Reached = deferred<void>();
    let refreshCallCount = 0;
    const oauth = makeFakeOauth({
      isExpired: vi.fn(() => true),
      refreshTokens: vi.fn(async () => {
        refreshCallCount += 1;
        if (refreshCallCount === 1) {
          call1Reached.resolve();
          return refreshGate.promise; // call #1's token step hangs here.
        }
        return { accessToken: 'call2-token', refreshToken: 'call2-refresh', expiresAt: 42 };
      }),
    });
    const m = new DiscordRpcMuter({
      createClient: () => {
        const c = new FakeRpcClient();
        clients.push(c);
        return c as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const p1 = m.connect('cid', 'secret', { accessToken: 'old', tokenExpiresAt: 1, refreshToken: 'r' });
    await call1Reached.promise; // call #1 is now hung inside refreshTokens.

    const ok2 = await m.connect('cid', 'secret', { accessToken: 'old2', tokenExpiresAt: 1, refreshToken: 'r2' });
    expect(ok2).toBe(true);
    expect(m.isConnected()).toBe(true);
    expect(m.getTokens()).toEqual({ accessToken: 'call2-token', refreshToken: 'call2-refresh', tokenExpiresAt: 42 });

    // Now let the stale call #1 finally settle.
    refreshGate.resolve({ accessToken: 'stale', refreshToken: 'stale-r', expiresAt: 1 });
    const ok1 = await p1;

    expect(ok1).toBe(false);
    // Call #1 must NOT have clobbered call #2's live state.
    expect(m.isConnected()).toBe(true);
    expect(m.getTokens()).toEqual({ accessToken: 'call2-token', refreshToken: 'call2-refresh', tokenExpiresAt: 42 });
  });

  it('keeps a rotated token even when authenticate() fails right after refresh', async () => {
    let capturedClient: FakeRpcClient | null = null;
    const oauth = makeFakeOauth({
      isExpired: vi.fn(() => true),
      refreshTokens: vi.fn(async () => ({ accessToken: 'new', refreshToken: 'r2', expiresAt: 123 })),
    });
    const m = new DiscordRpcMuter({
      createClient: () => {
        capturedClient = new FakeRpcClient();
        capturedClient.authenticate = async () => {
          throw new Error('discord dropped mid-authenticate');
        };
        return capturedClient as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const ok = await m.connect('cid', 'secret', { accessToken: 'old', tokenExpiresAt: 1, refreshToken: 'r' });

    expect(ok).toBe(false);
    expect(m.isConnected()).toBe(false);
    // The rotated token must survive so the caller can persist it — retrying
    // with the old (now dead, server-rotated) refresh token would otherwise
    // fall through to the AUTHORIZE popup.
    expect(m.getTokens()).toEqual({ accessToken: 'new', refreshToken: 'r2', tokenExpiresAt: 123 });
  });

  it('destroys the client when a post-connect step fails (authenticate rejects)', async () => {
    // client.connect() succeeds (the IPC socket is open) but authenticate()
    // rejects afterwards. The socket must not just be dereferenced — it has
    // to be destroy()ed, or a long-running, auto-retrying session leaks fds.
    let capturedClient: FakeRpcClient | null = null;
    const m = new DiscordRpcMuter({
      createClient: () => {
        capturedClient = new FakeRpcClient();
        capturedClient.authenticate = async () => {
          throw new Error('discord rejected the access token');
        };
        return capturedClient as any;
      },
      oauth: makeFakeOauth({ isExpired: vi.fn(() => false) }) as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const ok = await m.connect('cid', 'secret', { accessToken: 'tok', tokenExpiresAt: 999999 });

    expect(ok).toBe(false);
    expect(m.isConnected()).toBe(false);
    const client = capturedClient as unknown as FakeRpcClient;
    expect(client.destroyed).toBe(true);
  });

  it('re-arms the drop watchdog after a disconnect + reconnect', async () => {
    const clients: FakeRpcClient[] = [];
    const oauth = makeFakeOauth({ isExpired: vi.fn(() => false) });
    const m = new DiscordRpcMuter({
      createClient: () => {
        const c = new FakeRpcClient();
        clients.push(c);
        return c as any;
      },
      oauth: oauth as any,
      fetchImpl: (async () => {
        throw new Error('unused fetch');
      }) as any,
      now: () => 1000,
    });

    const onDrop = vi.fn();
    m.setOnDrop(onDrop);

    expect(await m.connect('cid', 'secret', { accessToken: 'tok', tokenExpiresAt: 999999 })).toBe(true);
    await m.disconnect();
    expect(await m.connect('cid', 'secret', { accessToken: 'tok2', tokenExpiresAt: 999999 })).toBe(true);

    const newClient = clients[clients.length - 1];
    newClient.fireDisconnected();

    expect(onDrop).toHaveBeenCalledTimes(1);
    expect(m.getState()).toBe('disconnected');
  });
});

describe('DiscordRpcMuter restores prior voice state (snapshot/restore)', () => {
  // A connected muter whose fake Discord starts in the given voice state.
  // `reconnect(nextVoice)` dials a fresh client seeded with nextVoice and returns
  // it — used to simulate the user changing Discord state while the bridge was
  // disconnected. `client` is the first client (valid until a reconnect).
  async function connectedMuter(voice: { mute?: boolean; deaf?: boolean }) {
    let current = voice;
    let client: FakeRpcClient | null = null;
    const m = new DiscordRpcMuter({
      createClient: () => {
        client = new FakeRpcClient();
        client.voiceSettings = { ...current };
        return client as any;
      },
      oauth: makeFakeOauth({ isExpired: vi.fn(() => false) }) as any,
      fetchImpl: (async () => { throw new Error('fetchImpl should not be called'); }) as any,
      now: () => 1000,
    });
    const connect = () => m.connect('cid', 'secret', { accessToken: 'tok', tokenExpiresAt: 999999 });
    await connect();
    const reconnect = async (nextVoice: { mute?: boolean; deaf?: boolean }) => {
      current = nextVoice;
      await connect();
      return client!;
    };
    return { m, client: client!, reconnect };
  }

  const sets = (client: FakeRpcClient) =>
    client.calls.filter((c) => c.name === 'setVoiceSettings').map((c) => c.args[0]);

  it('classic: not muted before -> mute on hold, unmute on release', async () => {
    const { m, client } = await connectedMuter({ mute: false, deaf: false });
    await m.setMute(true);
    await m.setMute(false);
    expect(sets(client)).toEqual([{ mute: true }, { mute: false, deaf: false }]);
  });

  it('stays muted on release if already self-muted before', async () => {
    const { m, client } = await connectedMuter({ mute: true, deaf: false });
    await m.setMute(true);
    await m.setMute(false);
    expect(sets(client)).toEqual([{ mute: true }, { mute: true, deaf: false }]);
  });

  it('stays deafened on release if deafened before (deaf preserved, never asserted during hold)', async () => {
    const { m, client } = await connectedMuter({ mute: true, deaf: true });
    await m.setMute(true);
    await m.setMute(false);
    // hold asserts only { mute: true } (no deaf field); release restores both.
    expect(sets(client)).toEqual([{ mute: true }, { mute: true, deaf: true }]);
  });

  it('snapshots only once across a double setMute(true) (idempotent hold)', async () => {
    const { m, client } = await connectedMuter({ mute: false, deaf: false });
    await m.setMute(true);
    await m.setMute(true);
    const snapshots = client.calls.filter((c) => c.name === 'getVoiceSettings');
    expect(snapshots.length).toBe(1);
  });

  it('fails closed when the prior voice state could not be read', async () => {
    const { m, client } = await connectedMuter({ mute: false, deaf: false });
    client.getVoiceSettings = (async () => { throw new Error('rpc query failed'); }) as any;
    await m.setMute(true);
    await m.setMute(false);
    expect(sets(client)).toEqual([{ mute: true }]);
  });

  it('re-snapshots after a mid-hold RPC drop instead of restoring stale state', async () => {
    const { m, client, reconnect } = await connectedMuter({ mute: false, deaf: false });
    await m.setMute(true);                              // snapshot { mute:false, deaf:false }
    client.fireDisconnected();                          // RPC drops mid-hold
    const client2 = await reconnect({ mute: true, deaf: true }); // user self-deafens while bridge is down
    await m.setMute(true);                              // must RE-snapshot the current { mute:true, deaf:true }
    await m.setMute(false);                             // restore -> must be current, not stale
    expect(sets(client2).pop()).toEqual({ mute: true, deaf: true });
  });

  it('does not unmute after a mid-hold drop cleared ownership', async () => {
    const { m, client, reconnect } = await connectedMuter({ mute: false, deaf: false });
    await m.setMute(true);
    client.fireDisconnected();
    const client2 = await reconnect({ mute: true, deaf: false });
    await m.setMute(false);
    expect(sets(client2)).toEqual([]);
  });
});
