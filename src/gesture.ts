import { dbg } from './debug';

// Injectable clock + timers so the state machine is deterministically testable.
export interface GestureDeps {
  now: () => number;
  setTimer: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimer: (t: ReturnType<typeof setTimeout>) => void;
}

const realDeps: GestureDeps = {
  now: () => Date.now(),
  setTimer: (fn, ms) => setTimeout(fn, ms),
  clearTimer: (t) => clearTimeout(t),
};

export interface GestureOptions {
  tapMaxMs?: number; // press→release shorter than this is a "tap"; longer is a "hold"
  doubleTapMs?: number; // second tap must start within this of the first tap's release
}

// Mirrors Willow Voice's dictation lifecycle from raw shortcut gestures, so
// Discord remains muted for the same interval that Willow is listening:
//   - HOLD the key              → active while held            (push-to-talk)
//   - DOUBLE-TAP the key        → active and latched           (hands-free start)
//   - single TAP while latched  → stop                         (hands-free stop)
// It calls onActivate/onDeactivate, which the orchestrator maps to Discord mute.
export class WillowGesture {
  private active = false;
  private latched = false;
  private stopping = false;
  private downAt: number | null = null;
  private awaitingSecond = false;
  private pendingTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly tapMaxMs: number;
  private readonly doubleTapMs: number;

  constructor(
    private readonly onActivate: () => void,
    private readonly onDeactivate: () => void,
    opts: GestureOptions = {},
    private readonly deps: GestureDeps = realDeps,
  ) {
    this.tapMaxMs = opts.tapMaxMs ?? 250;
    this.doubleTapMs = opts.doubleTapMs ?? 350;
  }

  isActive(): boolean {
    return this.active;
  }

  press(): void {
    this.clearPending();
    const t = this.deps.now();
    if (this.latched) {
      // A press while hands-free is latched begins the single stop-tap.
      this.downAt = t;
      this.stopping = true;
      return;
    }
    this.downAt = t;
    if (this.awaitingSecond) {
      // Second tap of a double-tap → latch on (stay muted after release).
      this.awaitingSecond = false;
      this.latched = true;
      dbg('gesture: double-tap → hands-free on');
      return; // already muted from the first tap's press
    }
    // Fresh press: mute now (instant push-to-talk; also the start of any double-tap).
    this.activate();
  }

  release(): void {
    const downAt = this.downAt;
    this.downAt = null;
    if (this.latched) {
      if (this.stopping) {
        this.stopping = false;
        this.latched = false;
        dbg('gesture: tap → hands-free off');
        this.deactivate();
      }
      return; // release of the latching tap: stay muted
    }
    if (downAt === null) return;
    if (this.deps.now() - downAt > this.tapMaxMs) {
      // It was a hold → push-to-talk release.
      dbg('gesture: hold release → off');
      this.deactivate();
      return;
    }
    // Quick tap: wait to see whether a second tap turns it into a double-tap.
    this.awaitingSecond = true;
    this.pendingTimer = this.deps.setTimer(() => {
      this.pendingTimer = null;
      this.awaitingSecond = false;
      if (!this.latched) this.deactivate(); // a lone tap just mutes briefly, then releases
    }, this.doubleTapMs);
  }

  // Watchdog / mode teardown: force everything off.
  reset(): void {
    this.clearPending();
    this.latched = false;
    this.stopping = false;
    this.downAt = null;
    this.awaitingSecond = false;
    this.deactivate();
  }

  private activate(): void {
    if (this.active) return;
    this.active = true;
    this.onActivate();
  }

  private deactivate(): void {
    if (!this.active) return;
    this.active = false;
    this.onDeactivate();
  }

  private clearPending(): void {
    if (this.pendingTimer) {
      this.deps.clearTimer(this.pendingTimer);
      this.pendingTimer = null;
    }
  }
}
