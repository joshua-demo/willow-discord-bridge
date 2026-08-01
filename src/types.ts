export type Mod = 'ctrl' | 'alt' | 'cmd' | 'shift';
export type Combo = { mods: Mod[]; key: string };

// auto mirrors Willow's shortcut gestures: hold = push-to-dictate, double-tap =
// locked dictation, and one tap while locked = stop. The app observes the same
// physical keys as Willow; it never injects keys into either application.
export type Mode = 'auto' | 'hold' | 'toggle';

export interface InputEngine {
  start(): void;
  stop(): void;
  onPress(callback: () => void): void;
  onRelease(callback: () => void): void;
}

export interface DiscordMuter {
  setMute(on: boolean): Promise<void>;
}

export interface DiscordRpc {
  clientId: string;
  clientSecret: string;
  accessToken?: string;
  refreshToken?: string;
  tokenExpiresAt?: number;
}

export interface BridgeConfig {
  shortcut: Combo;
  discordRpc: DiscordRpc;
  mode: Mode;
  unmuteDelayMs: number;
  launchAtLogin: boolean;
}
