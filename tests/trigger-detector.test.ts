import { describe, expect, it } from 'vitest';
import { UiohookKey } from 'uiohook-napi';
import { TriggerDetector, uiohookModifierOf } from '../src/input-engine';
import { Combo } from '../src/types';

type Edge = 'press' | 'release';

function detectorFor(trigger: Combo) {
  const edges: Edge[] = [];
  const detector = new TriggerDetector(trigger);
  detector.onPress(() => edges.push('press'));
  detector.onRelease(() => edges.push('release'));
  return { detector, edges };
}

describe('Windows native key mapping', () => {
  it('maps both Windows and Control keys from uiohook', () => {
    expect(uiohookModifierOf(UiohookKey.Meta)).toBe('cmd');
    expect(uiohookModifierOf(UiohookKey.MetaRight)).toBe('cmd');
    expect(uiohookModifierOf(UiohookKey.Ctrl)).toBe('ctrl');
    expect(uiohookModifierOf(UiohookKey.CtrlRight)).toBe('ctrl');
  });
});

describe('Ctrl+Windows modifier-only trigger', () => {
  const trigger: Combo = { mods: ['ctrl', 'cmd'], key: '' };

  it('activates once when both keys are held and releases when either lifts', () => {
    const { detector, edges } = detectorFor(trigger);
    detector.modDown('ctrl');
    detector.modDown('cmd');
    detector.modUp('ctrl');
    detector.modUp('cmd');
    expect(edges).toEqual(['press', 'release']);
  });

  it('does not activate with only one required key', () => {
    const { detector, edges } = detectorFor(trigger);
    detector.modDown('ctrl');
    detector.modUp('ctrl');
    expect(edges).toEqual([]);
  });

  it('stays active if an unrelated modifier is pressed during dictation', () => {
    const { detector, edges } = detectorFor(trigger);
    detector.modDown('ctrl');
    detector.modDown('cmd');
    detector.modDown('shift');
    detector.modUp('shift');
    detector.modUp('cmd');
    expect(edges).toEqual(['press', 'release']);
  });

  it('requires the exact combo to start when extra modifiers were already held', () => {
    const { detector, edges } = detectorFor(trigger);
    detector.modDown('shift');
    detector.modDown('ctrl');
    detector.modDown('cmd');
    expect(edges).toEqual([]);
    detector.modUp('shift');
    expect(edges).toEqual(['press']);
  });
});

describe('key-based trigger', () => {
  const trigger: Combo = { mods: ['ctrl', 'alt'], key: 'D' };

  it('fires with exact modifiers and ignores key auto-repeat', () => {
    const { detector, edges } = detectorFor(trigger);
    detector.modDown('ctrl');
    detector.modDown('alt');
    detector.keyDown('D');
    detector.keyDown('D');
    detector.keyUp('D');
    expect(edges).toEqual(['press', 'release']);
  });

  it('does not fire without all required modifiers', () => {
    const { detector, edges } = detectorFor(trigger);
    detector.modDown('ctrl');
    detector.keyDown('D');
    expect(edges).toEqual([]);
  });
});
