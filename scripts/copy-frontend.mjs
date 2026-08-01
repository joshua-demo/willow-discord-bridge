import { cp, mkdir } from 'node:fs/promises';
await mkdir('frontend-dist', { recursive: true });
await Promise.all([
  cp('frontend/index.html', 'frontend-dist/index.html'),
  cp('frontend/style.css', 'frontend-dist/style.css'),
  cp('assets', 'frontend-dist/assets', { recursive: true }),
]);
