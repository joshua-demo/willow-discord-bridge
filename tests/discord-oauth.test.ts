import { describe, it, expect, vi } from 'vitest';
import {
  exchangeCode,
  refreshTokens,
  computeExpiresAt,
  isExpired,
  FetchLike,
} from '../src/discord-oauth';

function fakeFetchOk(body: unknown) {
  return vi.fn(async () => ({
    ok: true,
    status: 200,
    json: async () => body,
  })) as unknown as FetchLike;
}

describe('exchangeCode', () => {
  it('POSTs form-encoded params to the Discord token endpoint and returns a TokenSet', async () => {
    const fetchImpl = fakeFetchOk({
      access_token: 'a',
      refresh_token: 'r',
      expires_in: 604800,
    });

    const result = await exchangeCode(
      fetchImpl,
      { clientId: 'cid', clientSecret: 'sec', code: 'code123', redirectUri: 'http://localhost' },
      1000,
    );

    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const [url, init] = (fetchImpl as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(url).toBe('https://discord.com/api/oauth2/token');
    expect(init.method).toBe('POST');
    expect(init.headers['Content-Type']).toBe('application/x-www-form-urlencoded');
    expect(init.body).toContain('grant_type=authorization_code');
    expect(init.body).toContain('code=code123');
    expect(init.body).toContain('client_id=cid');
    expect(init.body).toContain('client_secret=sec');
    expect(init.body).toContain('redirect_uri=');
    expect(init.body).toContain(`redirect_uri=${encodeURIComponent('http://localhost')}`);

    expect(result).toEqual({
      accessToken: 'a',
      refreshToken: 'r',
      expiresAt: 1000 + 604800 * 1000,
    });
  });
});

describe('refreshTokens', () => {
  it('POSTs a refresh_token grant and returns a TokenSet', async () => {
    const fetchImpl = fakeFetchOk({
      access_token: 'a2',
      refresh_token: 'r2',
      expires_in: 604800,
    });

    const result = await refreshTokens(
      fetchImpl,
      { clientId: 'cid', clientSecret: 'sec', refreshToken: 'old' },
      2000,
    );

    const [, init] = (fetchImpl as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(init.body).toContain('grant_type=refresh_token');
    expect(init.body).toContain('refresh_token=old');

    expect(result).toEqual({
      accessToken: 'a2',
      refreshToken: 'r2',
      expiresAt: 2000 + 604800 * 1000,
    });
  });
});

describe('error handling', () => {
  it('rejects on an HTTP error response', async () => {
    const fetchImpl = vi.fn(async () => ({
      ok: false,
      status: 400,
      json: async () => ({}),
    })) as unknown as FetchLike;

    await expect(
      exchangeCode(
        fetchImpl,
        { clientId: 'cid', clientSecret: 'sec', code: 'code123', redirectUri: 'http://localhost' },
        1000,
      ),
    ).rejects.toThrow();
  });

  it('rejects on a malformed response missing refresh_token', async () => {
    const fetchImpl = fakeFetchOk({ access_token: 'a' });

    await expect(
      exchangeCode(
        fetchImpl,
        { clientId: 'cid', clientSecret: 'sec', code: 'code123', redirectUri: 'http://localhost' },
        1000,
      ),
    ).rejects.toThrow();
  });
});

describe('computeExpiresAt', () => {
  it('adds expiresInSec (converted to ms) to nowMs', () => {
    expect(computeExpiresAt(604800, 1000)).toBe(1000 + 604800000);
  });
  it('returns nowMs when expiresInSec is 0', () => {
    expect(computeExpiresAt(0, 5)).toBe(5);
  });
  it('returns nowMs when expiresInSec is NaN', () => {
    expect(computeExpiresAt(NaN, 5)).toBe(5);
  });
  it('returns nowMs when expiresInSec is negative', () => {
    expect(computeExpiresAt(-10, 5)).toBe(5);
  });
});

describe('isExpired', () => {
  it('is true when expiresAt is undefined', () => {
    expect(isExpired(undefined, 1000)).toBe(true);
  });
  it('is true when nowMs is past expiresAt', () => {
    expect(isExpired(10_000, 9_999_999)).toBe(true);
  });
  it('is false when nowMs is well before expiresAt', () => {
    expect(isExpired(10_000_000, 1000)).toBe(false);
  });
  it('is true within the default 60s skew window', () => {
    expect(isExpired(100_000, 100_000 - 30_000)).toBe(true);
  });
  it('is false outside the default 60s skew window', () => {
    expect(isExpired(100_000, 100_000 - 90_000)).toBe(false);
  });
});
