import { Combo, Mod } from './types';

const MOD_ORDER: Mod[] = ['ctrl', 'alt', 'cmd', 'shift'];

export function normalizeMods(mods: Mod[]): Mod[] {
  return MOD_ORDER.filter((m) => mods.includes(m));
}

export function comboEquals(a: Combo, b: Combo): boolean {
  if (a.key.toUpperCase() !== b.key.toUpperCase()) return false;
  const na = normalizeMods(a.mods);
  const nb = normalizeMods(b.mods);
  return na.length === nb.length && na.every((m, i) => m === nb[i]);
}

export function combosDistinct(combos: Combo[]): boolean {
  for (let i = 0; i < combos.length; i++) {
    for (let j = i + 1; j < combos.length; j++) {
      if (comboEquals(combos[i], combos[j])) return false;
    }
  }
  return true;
}

const MOD_LABEL: Record<Mod, string> = {
  ctrl: 'Ctrl',
  alt: 'Alt',
  cmd: 'Win',
  shift: 'Shift',
};

export function comboLabel(combo: Combo): string {
  const parts = normalizeMods(combo.mods).map((modifier) => MOD_LABEL[modifier]);
  if (combo.key) parts.push(combo.key.toUpperCase());
  return parts.join(' + ') || 'Not set';
}
