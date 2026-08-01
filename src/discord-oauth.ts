// Pure, testable OAuth2 token exchange for Discord's RPC login. `discord-rpc`
// only hands us an access token and throws away the refresh token, so we do
// this exchange ourselves: plain form-encoded POSTs to Discord's token
// endpoint with client credentials in the body (NOT a Bearer header). Kept
// dependency-free (fetchImpl injected) so it can be unit tested without a
// network stack or Electron runtime.

export type TokenSet = { accessToken: string; refreshToken: string; expiresAt: number };

export type FetchLike = (
  url: string,
  init: { method: string; headers: Record<string, string>; body: string },
) => Promise<{ ok: boolean; status: number; json(): Promise<any> }>;

const TOKEN_URL = 'https://discord.com/api/oauth2/token';

async function post(
  fetchImpl: FetchLike,
  params: Record<string, string>,
  nowMs: number,
): Promise<TokenSet> {
  const res = await fetchImpl(TOKEN_URL, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams(params).toString(),
  });
  if (!res.ok) throw new Error(`token endpoint HTTP ${res.status}`);
  const body = await res.json();
  if (!body.access_token || !body.refresh_token) throw new Error('token response missing tokens');
  return {
    accessToken: String(body.access_token),
    refreshToken: String(body.refresh_token),
    expiresAt: computeExpiresAt(Number(body.expires_in), nowMs),
  };
}

// Initial authorization: exchange the OAuth `code` from the Discord consent
// popup for an access + refresh token pair.
export function exchangeCode(
  fetchImpl: FetchLike,
  opts: { clientId: string; clientSecret: string; code: string; redirectUri: string },
  nowMs: number,
): Promise<TokenSet> {
  return post(
    fetchImpl,
    {
      client_id: opts.clientId,
      client_secret: opts.clientSecret,
      grant_type: 'authorization_code',
      code: opts.code,
      redirect_uri: opts.redirectUri,
    },
    nowMs,
  );
}

// Silent renewal: exchange a stored refresh token for a fresh access token
// (and a new refresh token — Discord rotates it on every use).
export function refreshTokens(
  fetchImpl: FetchLike,
  opts: { clientId: string; clientSecret: string; refreshToken: string },
  nowMs: number,
): Promise<TokenSet> {
  return post(
    fetchImpl,
    {
      client_id: opts.clientId,
      client_secret: opts.clientSecret,
      grant_type: 'refresh_token',
      refresh_token: opts.refreshToken,
    },
    nowMs,
  );
}

export function computeExpiresAt(expiresInSec: number, nowMs: number): number {
  const secs = Number.isFinite(expiresInSec) && expiresInSec > 0 ? expiresInSec : 0;
  return nowMs + secs * 1000;
}

// True when the token is already expired or will expire within `skewMs`
// (default 60s) — i.e. it's not safe to use without refreshing first.
export function isExpired(expiresAt: number | undefined, nowMs: number, skewMs = 60_000): boolean {
  if (!expiresAt) return true;
  return nowMs >= expiresAt - skewMs;
}
