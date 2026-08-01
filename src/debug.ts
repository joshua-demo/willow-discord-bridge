import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

const marker = path.join(os.homedir(), '.willow-bridge-debug');
const localAppData = process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
export const LOG_FILE = path.join(localAppData, 'Willow Discord Bridge', 'bridge-debug.log');

export const DEBUG =
  process.env.WILLOW_BRIDGE_DEBUG === '1' ||
  process.env.WILLOW_BRIDGE_DEBUG === 'true' ||
  (() => { try { return fs.existsSync(marker); } catch { return false; } })();

let logReady = false;
function ensureLog(): void {
  if (logReady) return;
  try {
    fs.mkdirSync(path.dirname(LOG_FILE), { recursive: true });
    fs.writeFileSync(LOG_FILE, `=== Willow Discord Bridge ${new Date().toISOString()} ===\n`);
    logReady = true;
  } catch { /* console logging remains available */ }
}

export function dbg(...args: unknown[]): void {
  if (!DEBUG) return;
  const line = `[willow-bridge] ${new Date().toISOString()} ${args.map((value) => {
    if (typeof value === 'string') return value;
    try { return JSON.stringify(value); } catch { return String(value); }
  }).join(' ')}`;
  console.error(line);
  ensureLog();
  if (logReady) {
    try { fs.appendFileSync(LOG_FILE, `${line}\n`); } catch { /* no-op */ }
  }
}
