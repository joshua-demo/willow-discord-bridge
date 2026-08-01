import { describe, it, expect } from 'vitest';
import { WillowGesture } from '../src/gesture';

// A controllable clock + timer queue so gestures are fully deterministic.
function harness() {
  let t = 0;
  let id = 0;
  const timers: { fn: () => void; at: number; id: number }[] = [];
  const events: string[] = [];
  const deps = {
    now: () => t,
    setTimer: (fn: () => void, ms: number) => {
      const tid = ++id;
      timers.push({ fn, at: t + ms, id: tid });
      return tid as unknown as ReturnType<typeof setTimeout>;
    },
    clearTimer: (tid: ReturnType<typeof setTimeout>) => {
      const i = timers.findIndex((x) => x.id === (tid as unknown as number));
      if (i >= 0) timers.splice(i, 1);
    },
  };
  const advance = (ms: number) => {
    t += ms;
    for (const timer of [...timers]) {
      if (timer.at <= t) {
        timers.splice(timers.indexOf(timer), 1);
        timer.fn();
      }
    }
  };
  const g = new WillowGesture(
    () => events.push('mute'),
    () => events.push('unmute'),
    { tapMaxMs: 250, doubleTapMs: 350 },
    deps,
  );
  return { g, events, advance };
}

describe('WillowGesture — push-to-talk (hold)', () => {
  it('mutes while held, unmutes on release', () => {
    const { g, events, advance } = harness();
    g.press();
    advance(1000); // held for 1s
    g.release();
    expect(events).toEqual(['mute', 'unmute']);
    expect(g.isActive()).toBe(false);
  });
});

describe('WillowGesture — lone tap', () => {
  it('mutes briefly then unmutes when no second tap follows', () => {
    const { g, events, advance } = harness();
    g.press();
    advance(80);
    g.release(); // quick tap
    expect(events).toEqual(['mute']); // still muted, waiting for a possible double-tap
    advance(350); // double-tap window elapses with no second tap
    expect(events).toEqual(['mute', 'unmute']);
    expect(g.isActive()).toBe(false);
  });
});

describe('WillowGesture — hands-free (double-tap → tap)', () => {
  it('double-tap latches muted through release; a single tap later stops', () => {
    const { g, events, advance } = harness();
    // First tap
    g.press();
    advance(60);
    g.release();
    // Second tap within the window → latch
    advance(120);
    g.press();
    advance(60);
    g.release();
    // Let plenty of time pass — must STAY muted (hands-free), key released.
    advance(5000);
    expect(events).toEqual(['mute']);
    expect(g.isActive()).toBe(true);

    // A single tap now stops it.
    g.press();
    advance(60);
    g.release();
    expect(events).toEqual(['mute', 'unmute']);
    expect(g.isActive()).toBe(false);
  });

  it('a hold still works right after a hands-free session', () => {
    const { g, events, advance } = harness();
    // hands-free on
    g.press(); advance(50); g.release();
    advance(100); g.press(); advance(50); g.release();
    advance(1000);
    // stop
    g.press(); advance(50); g.release();
    events.length = 0;
    // now a normal push-to-talk hold
    g.press();
    advance(800);
    g.release();
    expect(events).toEqual(['mute', 'unmute']);
  });
});

describe('WillowGesture — reset (watchdog)', () => {
  it('force-unmutes when latched', () => {
    const { g, events, advance } = harness();
    g.press(); advance(50); g.release();
    advance(100); g.press(); advance(50); g.release(); // latched
    expect(g.isActive()).toBe(true);
    g.reset();
    expect(events).toEqual(['mute', 'unmute']);
    expect(g.isActive()).toBe(false);
  });
});
