import { DiscordMuter, BridgeConfig } from './types';
import { WillowGesture, GestureDeps } from './gesture';
import { dbg } from './debug';

const realSleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// While Willow listens to its configured shortcut, mute Discord and restore
// its previous state afterward. This never controls Willow; both apps observe
// the same physical shortcut independently.
export class Orchestrator {
  private active = false;
  // Presses/releases fire from the global hook as fire-and-forget callbacks, and
  // each mute/unmute is async (an RPC round-trip). Serialize them through this
  // tail so a release can't overtake the mute it's meant to undo.
  private queue: Promise<void> = Promise.resolve();
  // In hands-free mode a gesture recognizer turns taps/holds/double-taps into
  // activate/deactivate; in hold/toggle modes it stays null.
  private readonly gesture: WillowGesture | null;

  constructor(
    private readonly discord: DiscordMuter,
    private readonly cfg: BridgeConfig,
    private readonly onActiveChange?: (active: boolean) => void,
    private readonly sleep: (ms: number) => Promise<void> = realSleep,
    gestureDeps?: GestureDeps,
  ) {
    this.gesture =
      cfg.mode === 'auto'
        ? new WillowGesture(
            () => void this.enqueue(() => this.activate()),
            () => void this.enqueue(() => this.deactivate()),
            {},
            gestureDeps,
          )
        : null;
  }

  // Await all queued mute/unmute transitions — for deterministic tests.
  whenIdle(): Promise<void> {
    return this.queue;
  }

  // Chain a transition onto the queue; keep the chain alive even if one throws so
  // a single failed transition can't wedge every later press/release.
  private enqueue(task: () => Promise<void>): Promise<void> {
    const next = this.queue.then(task, task);
    this.queue = next.catch(() => {});
    return next;
  }

  async onPress(): Promise<void> {
    if (this.gesture) { this.gesture.press(); return; }
    return this.enqueue(() =>
      this.cfg.mode === 'hold'
        ? this.activate()
        : this.active
          ? this.deactivate()
          : this.activate(),
    );
  }

  async onRelease(): Promise<void> {
    if (this.gesture) { this.gesture.release(); return; }
    return this.enqueue(() => (this.cfg.mode === 'hold' ? this.deactivate() : Promise.resolve()));
  }

  async forceRelease(): Promise<void> {
    if (this.gesture) { this.gesture.reset(); return; }
    return this.enqueue(() => (this.active ? this.deactivate() : Promise.resolve()));
  }

  isActive(): boolean {
    return this.active;
  }

  private setActive(value: boolean): void {
    this.active = value;
    this.onActiveChange?.(value);
  }

  private async activate(): Promise<void> {
    if (this.active) return;
    dbg('orchestrator: activate (mute)');
    this.setActive(true);
    await this.discord.setMute(true);
  }

  private async deactivate(): Promise<void> {
    if (!this.active) return;
    this.setActive(false);
    // Restoring the user's prior voice state (incl. a pre-existing mute/deafen)
    // now lives in the muter, so we always ask to unmute and let it decide.
    dbg('orchestrator: deactivate (unmute)');
    if (this.cfg.unmuteDelayMs > 0) await this.sleep(this.cfg.unmuteDelayMs);
    await this.discord.setMute(false);
  }
}
