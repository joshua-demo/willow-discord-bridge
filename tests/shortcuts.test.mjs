import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { JSDOM } from 'jsdom';

async function setup(t, rejectSave = false) {
  const dom = new JSDOM(await readFile('frontend/index.html', 'utf8'), { runScripts: 'outside-only' });
  t.after(() => dom.window.close());
  const calls = [];
  const config = {
    dictationApp: 'wispr', shortcut: { mods: ['ctrl', 'cmd'], key: '' },
    handsFreeShortcut: { mods: ['ctrl', 'cmd'], key: 'VK_20' },
    discordRpc: { clientId: '', clientSecret: '' }, mode: 'auto',
    deafenWhileActive: false, soundboardSoundId: '', soundboardGuildIds: [],
    unmuteDelayMs: 0, launchAtLogin: true,
  };
  dom.window.structuredClone = structuredClone;
  dom.window.__TAURI__ = {
    core: { invoke: async (command, args) => {
      calls.push({ command, args });
      if (command === 'get_config') return structuredClone(config);
      if (command === 'get_status') return { engineReady: true, active: false, rpc: 'disconnected' };
      if (command === 'get_version') return 'test';
      if (command === 'save_config') return rejectSave ? { ok: false, error: 'Save rejected' } : { ok: true, config: args.config };
      throw new Error(`Unexpected command: ${command}`);
    } },
    event: { listen: async () => () => {} },
  };
  dom.window.eval((await readFile('frontend-dist/renderer.js', 'utf8')).replace(/export \{\};?\s*$/, ''));
  await new Promise((resolve) => setTimeout(resolve, 20));
  return { document: dom.window.document, calls };
}

test('manual Ctrl + backtick saves VK_C0 without invoking capture or changing hands-free', async (t) => {
  const { document, calls } = await setup(t);
  const picker = document.querySelector('.shortcut-picker');
  picker.open = true;
  await new Promise((resolve) => setTimeout(resolve, 20));
  const boxes = picker.querySelectorAll('input');
  assert.equal(boxes[0].checked, true);
  boxes[2].checked = false;
  picker.querySelector('select').value = 'VK_C0';
  picker.querySelector('button').click();
  await new Promise((resolve) => setTimeout(resolve, 20));
  const saved = calls.find(({ command }) => command === 'save_config').args.config;
  assert.deepEqual(saved.shortcut, { mods: ['ctrl'], key: 'VK_C0' });
  assert.deepEqual(saved.handsFreeShortcut, { mods: ['ctrl', 'cmd'], key: 'VK_20' });
  assert.equal(calls.some(({ command }) => command === 'capture_shortcut'), false);
  assert.equal(document.querySelector('#capture-shortcut').textContent, 'Ctrl + ` (Backtick)');
  assert.equal(picker.open, false);
});

test('manual picker rejects empty shortcuts and preserves the previous binding on save failure', async (t) => {
  const { document, calls } = await setup(t, true);
  const picker = document.querySelector('.shortcut-picker');
  picker.open = true;
  await new Promise((resolve) => setTimeout(resolve, 20));
  for (const box of picker.querySelectorAll('input')) box.checked = false;
  picker.querySelector('button').click();
  assert.equal(calls.some(({ command }) => command === 'save_config'), false);
  assert.match(document.querySelector('#error').textContent, /Choose at least/);
  picker.querySelector('select').value = 'VK_C0';
  picker.querySelector('button').click();
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(document.querySelector('#capture-shortcut').textContent, 'Ctrl + Win');
  assert.equal(document.querySelector('#error').textContent, 'Save rejected');
  assert.equal(picker.open, true);
});
