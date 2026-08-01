import { DiscordMuter } from './types';
import { dbg } from './debug';
import * as realOauth from './discord-oauth';
import { TokenSet, FetchLike } from './discord-oauth';

// Real-flow Discord muting over the local RPC/IPC socket — the approach that
// actually works, unlike synthesizing Discord's mute hotkey (Discord ignores
// session-level CGEvents). Talks to Discord via `SET_VOICE_SETTINGS { mute }`.
//
// Best-effort by design: if Discord is closed or the RPC never connected,
// setMute is a logged no-op so nothing breaks.
//
// Auth: we keep a long-lived refresh token around (persisted by the caller)
// and use it to renew silently — no popup after the very first authorize.
// We also watch for the transport dropping (Discord quitting, socket dying)
// so the caller can auto-reconnect instead of discovering it lazily on the
// next setMute call.

export interface RpcClient {
  connect(clientId: string): Promise<unknown>;
  request(cmd: string, args: Record<string, unknown>): Promise<any>;
  authenticate(accessToken: string): Promise<unknown>;
  setVoiceSettings(settings: { mute?: boolean; deaf?: boolean }): Promise<unknown>;
  getVoiceSettings?(): Promise<{ mute?: boolean; deaf?: boolean }>;
  destroy(): Promise<void>;
  on(event: string, cb: (...args: unknown[]) => void): void;
}

interface OauthDeps {
  exchangeCode: typeof realOauth.exchangeCode;
  refreshTokens: typeof realOauth.refreshTokens;
  isExpired: typeof realOauth.isExpired;
}

interface Deps {
  createClient: () => RpcClient;
  oauth: OauthDeps;
  fetchImpl: FetchLike;
  now: () => number;
}

export type RpcState = 'disconnected' | 'connecting' | 'connected';

export type ConnectTokens = {
  accessToken?: string;
  refreshToken?: string;
  tokenExpiresAt?: number;
};

const SCOPES = ['rpc', 'rpc.voice.write'];
const REDIRECT = 'http://localhost';
// A silent (cached-token) login is pure machinery — bound it tightly. The full
// OAuth authorize waits for the user to click "Authorize" in Discord's popup, so
// it needs a generous window (killing it early was breaking first-time connects,
// especially on Windows) — but still bounded so a wedged socket can't hang forever.
const TOKEN_LOGIN_TIMEOUT_MS = 12000;
const AUTHORIZE_TIMEOUT_MS = 120000;
const CONNECT_TIMEOUT_MS = 10000;

function withTimeout<T>(p: Promise<T>, ms: number, label: string): Promise<T> {
  return Promise.race([
    p,
    new Promise<T>((_, reject) =>
      setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms),
    ),
  ]);
}

// discord-rpc ships no bundled types and pulls a native transport; require it
// lazily so the app still boots if the dependency is somehow missing.
function defaultCreateClient(): RpcClient {
  const RPC = require('discord-rpc');
  return new RPC.Client({ transport: 'ipc' });
}

export class DiscordRpcMuter implements DiscordMuter {
  private deps: Deps;
  private client: RpcClient | null = null;
  private state: RpcState = 'disconnected';
  private lastError: string | null = null;
  private tokens: TokenSet | null = null;
  private closing = false;
  private onDrop: (() => void) | null = null;
  // Bumped at the start of every connect() attempt. Lets a superseded attempt
  // (overlapping connect() calls, or a socket drop mid-handshake) recognize
  // it's stale and bail out without clobbering a newer attempt's state.
  private generation = 0;
  // Snapshot/restore of the user's own voice state so a hold/release returns them
  // to exactly what they had before the bridge muted — covering self-mute and deafen.
  // heldByBridge gates the snapshot to the entering edge (idempotent under repeats).
  private heldByBridge = false;
  private priorState: { mute: boolean; deaf: boolean } | null = null;

  constructor(deps: Partial<Deps> = {}) {
    this.deps = {
      createClient: deps.createClient ?? defaultCreateClient,
      oauth: deps.oauth ?? {
        exchangeCode: realOauth.exchangeCode,
        refreshTokens: realOauth.refreshTokens,
        isExpired: realOauth.isExpired,
      },
      fetchImpl: deps.fetchImpl ?? ((globalThis.fetch as unknown) as FetchLike),
      now: deps.now ?? (() => Date.now()),
    };
  }

  getState(): RpcState {
    return this.state;
  }

  getError(): string | null {
    return this.lastError;
  }

  // The full token set obtained (or reused/rotated) on the last successful
  // connect. Persist this so the next launch reconnects silently — no Discord
  // authorize prompt, and no unnecessary refresh call if the token is fresh.
  getTokens(): { accessToken: string; refreshToken: string; tokenExpiresAt: number } | null {
    if (!this.tokens) return null;
    return {
      accessToken: this.tokens.accessToken,
      refreshToken: this.tokens.refreshToken,
      tokenExpiresAt: this.tokens.expiresAt,
    };
  }

  // Backward-compat accessor.
  getAccessToken(): string | null {
    return this.tokens?.accessToken ?? null;
  }

  isConnected(): boolean {
    return this.state === 'connected';
  }

  // Register a callback fired when the RPC transport drops unexpectedly
  // (Discord quit, socket died) so the caller can auto-reconnect. Not called
  // on an intentional disconnect().
  setOnDrop(cb: () => void): void {
    this.onDrop = cb;
  }

  private handleDrop(): void {
    // Only a genuine drop of a *live* session counts. A socket close mid
    // handshake (still 'connecting') is not a drop of anything — the
    // in-flight connect() attempt already handles its own failure.
    if (this.state !== 'connected') return;
    if (this.closing) return;
    this.state = 'disconnected';
    this.client = null;
    this.clearHold();
    dbg('rpc: disconnected (drop)');
    this.onDrop?.();
  }

  // Connect (or reconnect). Never throws — failures are captured in
  // state/error so the caller can surface them without breaking anything.
  //
  // Token acquisition, in priority order (cheapest/quietest first):
  //   1. Cached access token, still valid → use it directly, no network.
  //   2. Refresh token present → silent renewal via the token endpoint.
  //   3. Neither (or refresh failed) → full OAuth authorize, which pops
  //      Discord's approval popup the first time only.
  async connect(clientId: string, clientSecret: string, tokens?: ConnectTokens): Promise<boolean> {
    if (!clientId || !clientSecret) {
      this.state = 'disconnected';
      this.lastError = 'Discord Client ID and Client Secret are required';
      return false;
    }
    // Claim this attempt's generation. Any earlier in-flight connect() (or a
    // watchdog bound to an earlier client) that resolves/fires after this
    // point will see a stale gen and no-op instead of clobbering our state.
    const gen = ++this.generation;
    this.state = 'connecting';
    this.lastError = null;
    await this.disconnect();
    // A newer connect() may have started while we awaited the teardown above —
    // bail before touching this.client so the newest attempt owns the socket.
    if (gen !== this.generation) return false;
    this.closing = false;

    // Hoisted above the try so a failure AFTER a successful client.connect()
    // (e.g. authenticate() rejects, or exchangeCode() throws post-AUTHORIZE)
    // can still reach the client in the catch block and destroy() it —
    // otherwise the open IPC socket is merely dereferenced and leaks an fd.
    let client: RpcClient | null = null;
    try {
      client = this.deps.createClient();
      this.client = client;
      client.on('disconnected', () => {
        if (gen === this.generation) this.handleDrop();
      });

      await withTimeout(client.connect(clientId), CONNECT_TIMEOUT_MS, 'rpc connect');
      if (gen !== this.generation) {
        try { client.destroy(); } catch { /* noop */ }
        return false;
      }

      let ts: TokenSet | null = null;
      if (tokens?.accessToken && !this.deps.oauth.isExpired(tokens.tokenExpiresAt, this.deps.now())) {
        // 1) Cached token still valid — no network call needed.
        ts = {
          accessToken: tokens.accessToken,
          refreshToken: tokens.refreshToken ?? '',
          expiresAt: tokens.tokenExpiresAt ?? 0,
        };
      } else {
        if (tokens?.refreshToken) {
          // 2) Silent renewal via the refresh token.
          try {
            ts = await this.deps.oauth.refreshTokens(
              this.deps.fetchImpl,
              { clientId, clientSecret, refreshToken: tokens.refreshToken },
              this.deps.now(),
            );
          } catch (err) {
            dbg('rpc: token refresh failed, falling back to authorize',
              err instanceof Error ? err.message : String(err));
          }
        }
        if (!ts) {
          // 3) Full OAuth authorize (prompts the first time / when refresh failed).
          const { code } = await withTimeout(
            client.request('AUTHORIZE', { scopes: SCOPES, client_id: clientId }),
            AUTHORIZE_TIMEOUT_MS,
            'rpc authorize',
          );
          ts = await this.deps.oauth.exchangeCode(
            this.deps.fetchImpl,
            { clientId, clientSecret, code, redirectUri: REDIRECT },
            this.deps.now(),
          );
        }
      }
      if (gen !== this.generation) {
        try { client.destroy(); } catch { /* noop */ }
        return false;
      }
      if (!ts) throw new Error('token acquisition failed');

      // Store the (possibly rotated) token immediately, BEFORE authenticate().
      // If Discord already rotated the refresh token server-side and
      // authenticate() then fails (e.g. the socket drops right then), the new
      // token is still captured here so the caller can persist it — instead
      // of silently losing it and retrying with a now-dead refresh token.
      if (gen === this.generation) this.tokens = ts;

      await withTimeout(client.authenticate(ts.accessToken), TOKEN_LOGIN_TIMEOUT_MS, 'rpc authenticate');
      if (gen !== this.generation) {
        try { client.destroy(); } catch { /* noop */ }
        return false;
      }

      this.state = 'connected';
      dbg('rpc: connected');
      return true;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      // A post-connect failure (authenticate() rejected, token exchange threw,
      // etc.) still leaves the IPC socket open — destroy it so it isn't just
      // dereferenced and leaked as a dangling fd.
      if (client) { try { client.destroy(); } catch { /* noop */ } }
      if (gen === this.generation) {
        this.client = null;
        this.state = 'disconnected';
        this.lastError = msg;
      }
      dbg('rpc: connect failed', msg);
      return false;
    }
  }

  // The user's current { mute, deaf } self-state, or null if we can't tell
  // (not connected / query unsupported / failed). Never throws.
  private async readVoiceState(): Promise<{ mute: boolean; deaf: boolean } | null> {
    if (!this.client || this.state !== 'connected' || !this.client.getVoiceSettings) return null;
    try {
      const s = await this.client.getVoiceSettings();
      return { mute: s.mute === true, deaf: s.deaf === true };
    } catch (err) {
      dbg('rpc: readVoiceState failed', err instanceof Error ? err.message : String(err));
      return null;
    }
  }

  // Forget any outstanding hold snapshot. Called on every teardown/release so a
  // later hold re-snapshots the current state instead of restoring a stale one.
  private clearHold(): void {
    this.heldByBridge = false;
    this.priorState = null;
  }

  // Mute for dictation, remembering the user's prior voice state so release can
  // restore it. On the entering edge we snapshot { mute, deaf } and assert only
  // mute:true (deaf is left untouched during the hold). On release we restore the
  // exact snapshot — so someone already muted or deafened is left that way. A
  // failed snapshot degrades to a plain unmute (best-effort, never worse).
  async setMute(on: boolean): Promise<void> {
    if (!this.client || this.state !== 'connected') {
      dbg('rpc: setMute skipped (not connected)', { on });
      return;
    }
    try {
      if (on) {
        if (!this.heldByBridge) {
          this.priorState = await this.readVoiceState();
          this.heldByBridge = true;
        }
        await this.client.setVoiceSettings({ mute: true });
        dbg('rpc: setMute', { on: true });
      } else {
        // Only restore when this live session owns a mute and successfully read
        // the state that preceded it. If the snapshot failed—or the RPC dropped
        // and reconnected mid-hold—unmuting would risk exposing a user who had
        // intentionally muted themselves. Fail closed instead.
        if (!this.heldByBridge) {
          dbg('rpc: release skipped (bridge does not own mute)');
          return;
        }
        const prior = this.priorState;
        this.clearHold();
        if (prior) {
          await this.client.setVoiceSettings({ mute: prior.mute, deaf: prior.deaf });
          dbg('rpc: restore prior voice state', prior);
        } else {
          dbg('rpc: release skipped (prior voice state unknown)');
        }
      }
    } catch (err) {
      // A dropped socket (Discord quit mid-session) lands here — degrade to
      // disconnected so the next attempt reconnects instead of throwing.
      this.state = 'disconnected';
      this.lastError = err instanceof Error ? err.message : String(err);
      dbg('rpc: setMute failed', this.lastError);
    }
  }

  async disconnect(): Promise<void> {
    this.closing = true;
    if (this.client) {
      try { await this.client.destroy(); } catch { /* noop */ }
    }
    this.client = null;
    if (this.state === 'connected') this.state = 'disconnected';
    this.clearHold();
  }
}
