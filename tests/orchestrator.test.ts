import { describe, it, expect } from 'vitest';
import { Orchestrator } from '../src/orchestrator';
import { DiscordMuter, BridgeConfig } from '../src/types';
import { DEFAULT_CONFIG } from '../src/config';

// Records the exact order of Discord mute/unmute calls.
class FakeMuter implements DiscordMuter {
  constructor(readonly calls: string[]) {}
  async setMute(on: boolean) { this.calls.push(`mute:${on}`); }
}

function rig(over: Partial<BridgeConfig> = {}, sleep?: (ms: number) => Promise<void>) {
  const calls: string[] = [];
  const cfg: BridgeConfig = {
    ...DEFAULT_CONFIG,
    mode: 'hold',
    unmuteDelayMs: 0,
    ...over,
  };
  const o = new Orchestrator(new FakeMuter(calls), cfg, undefined, sleep);
  return { calls, o };
}

describe('Orchestrator hold mode', () => {
  it('press mutes Discord; release unmutes', async () => {
    const { calls, o } = rig();
    await o.onPress();
    await o.onRelease();
    expect(calls).toEqual(['mute:true', 'mute:false']);
  });
  it('ignores a duplicated press while active (no double mute)', async () => {
    const { calls, o } = rig();
    await o.onPress();
    await o.onPress();
    expect(calls).toEqual(['mute:true']);
  });
});

describe('Orchestrator transition serialization', () => {
  // A modifier-only shortcut releases only milliseconds after it presses. If the
  // async mute/unmute weren't serialized, a release arriving mid-mute could run
  // its unmute before the mute resolved and leave Discord muted for good.
  it('a release during the mute round-trip runs AFTER the mute completes', async () => {
    // Real timer so the mute actually yields the event loop.
    const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
    const calls: string[] = [];
    const cfg: BridgeConfig = {
      ...DEFAULT_CONFIG,
      mode: 'hold',
      unmuteDelayMs: 10,
    };
    const o = new Orchestrator(new FakeMuter(calls), cfg, undefined, sleep);
    const press = o.onPress();
    const release = o.onRelease(); // arrives while the (delayed) transition is pending
    await Promise.all([press, release]);
    expect(calls).toEqual(['mute:true', 'mute:false']);
  });
});

describe('Orchestrator unmute delay', () => {
  it('waits unmuteDelayMs before unmuting on release', async () => {
    const calls: string[] = [];
    const sleep = async (ms: number) => { calls.push(`sleep:${ms}`); };
    const { o } = { o: new Orchestrator(new FakeMuter(calls), {
      ...DEFAULT_CONFIG,
      mode: 'hold',
      unmuteDelayMs: 40,
    }, undefined, sleep) };
    await o.onPress();
    await o.onRelease();
    expect(calls).toEqual(['mute:true', 'sleep:40', 'mute:false']);
  });
});

describe('Orchestrator toggle mode', () => {
  it('first press mutes, release is ignored, second press unmutes', async () => {
    const { calls, o } = rig({ mode: 'toggle' });
    await o.onPress();
    await o.onRelease();
    await o.onPress();
    expect(calls).toEqual(['mute:true', 'mute:false']);
  });
});

describe('Orchestrator forceRelease (watchdog)', () => {
  it('unmutes when active so we never stay muted', async () => {
    const { calls, o } = rig();
    await o.onPress();
    await o.forceRelease();
    expect(o.isActive()).toBe(false);
    expect(calls).toEqual(['mute:true', 'mute:false']);
  });
  it('is a no-op when idle', async () => {
    const { calls, o } = rig();
    await o.forceRelease();
    expect(calls).toEqual([]);
  });
});

describe('Orchestrator auto mode (Willow gesture mirror)', () => {
  // Fake clock + timer queue shared with the orchestrator's gesture recognizer.
  function autoRig() {
    let t = 0;
    let id = 0;
    const timers: { fn: () => void; at: number; id: number }[] = [];
    const deps = {
      now: () => t,
      setTimer: (fn: () => void, ms: number) => {
        const tid = ++id; timers.push({ fn, at: t + ms, id: tid });
        return tid as unknown as ReturnType<typeof setTimeout>;
      },
      clearTimer: (tid: ReturnType<typeof setTimeout>) => {
        const i = timers.findIndex((x) => x.id === (tid as unknown as number));
        if (i >= 0) timers.splice(i, 1);
      },
    };
    const advance = (ms: number) => {
      t += ms;
      for (const timer of [...timers]) if (timer.at <= t) { timers.splice(timers.indexOf(timer), 1); timer.fn(); }
    };
    const calls: string[] = [];
    const cfg: BridgeConfig = {
      shortcut: { mods: ['ctrl', 'alt', 'shift'], key: '' },
      discordRpc: { clientId: '', clientSecret: '' },
      mode: 'auto',
      unmuteDelayMs: 0,
    };
    const o = new Orchestrator(new FakeMuter(calls), cfg, undefined, undefined, deps);
    return { o, calls, advance };
  }

  it('HOLD mutes while held, unmutes on release (push-to-talk)', async () => {
    const { o, calls, advance } = autoRig();
    await o.onPress();
    advance(1000);
    await o.onRelease();
    await o.whenIdle();
    expect(calls).toEqual(['mute:true', 'mute:false']);
  });

  it('DOUBLE-TAP latches muted through release; a single TAP then stops', async () => {
    const { o, calls, advance } = autoRig();
    // double-tap
    await o.onPress(); advance(60); await o.onRelease();
    advance(120); await o.onPress(); advance(60); await o.onRelease();
    advance(5000); // long time, key released — must stay muted
    await o.whenIdle();
    expect(calls).toEqual(['mute:true']);
    // single tap stops
    await o.onPress(); advance(60); await o.onRelease();
    await o.whenIdle();
    expect(calls).toEqual(['mute:true', 'mute:false']);
  });

  it('a lone TAP mutes briefly then unmutes (no latch)', async () => {
    const { o, calls, advance } = autoRig();
    await o.onPress(); advance(80); await o.onRelease();
    await o.whenIdle();
    expect(calls).toEqual(['mute:true']);
    advance(350); // double-tap window passes with no second tap
    await o.whenIdle();
    expect(calls).toEqual(['mute:true', 'mute:false']);
  });
});

describe('Orchestrator onActiveChange', () => {
  it('notifies on activate and deactivate', async () => {
    const calls: string[] = [];
    const seen: boolean[] = [];
    const o = new Orchestrator(new FakeMuter(calls), {
      ...DEFAULT_CONFIG,
      mode: 'hold',
      unmuteDelayMs: 0,
    }, (a) => seen.push(a));
    await o.onPress();
    await o.onRelease();
    expect(seen).toEqual([true, false]);
  });
});
