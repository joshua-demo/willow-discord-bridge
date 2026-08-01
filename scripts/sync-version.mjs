import { readFile, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';

const pkg = JSON.parse(await readFile('package.json', 'utf8'));
const version = pkg.version;

const cargoPath = 'src-tauri/Cargo.toml';
const cargo = await readFile(cargoPath, 'utf8');
await writeFile(cargoPath, cargo.replace(/(\[package\][\s\S]*?\nversion = ")[^"]+"/, `$1${version}"`));

const tauriPath = 'src-tauri/tauri.conf.json';
const tauri = JSON.parse(await readFile(tauriPath, 'utf8'));
tauri.version = version;
await writeFile(tauriPath, `${JSON.stringify(tauri, null, 2)}\n`);

execFileSync('cargo', ['check', '--manifest-path', cargoPath], { stdio: 'inherit' });
execFileSync('git', ['add', cargoPath, tauriPath, 'src-tauri/Cargo.lock'], { stdio: 'inherit' });
