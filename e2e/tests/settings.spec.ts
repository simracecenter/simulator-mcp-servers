import { test, expect } from '@playwright/test';
import * as net from 'net';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { spawn, ChildProcess } from 'child_process';

const LAUNCHER = path.resolve(
  __dirname,
  '..',
  '..',
  'target',
  'debug',
  'simracecenter-launcher'
);

function getFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      const port = typeof address === 'object' && address ? address.port : 0;
      server.close(() => resolve(port));
    });
  });
}

function tmpAppDataDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'smcp-e2e-'));
}

async function waitForServer(port: number, timeoutMs = 15000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  const url = `http://127.0.0.1:${port}/healthz`;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(url);
      if (res.ok) return;
    } catch {
      // not ready yet
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`settings server did not become ready on port ${port}`);
}

async function fetchStatus(port: number): Promise<{ sim: string; connected: boolean; toolNames: string[] }> {
  const res = await fetch(`http://127.0.0.1:${port}/api/status`);
  if (!res.ok) throw new Error(`unexpected status ${res.status} from /api/status`);
  return (await res.json()) as { sim: string; connected: boolean; toolNames: string[] };
}

async function postSim(port: number, sim: string): Promise<{ sim: string; connected: boolean; toolNames: string[] }> {
  const res = await fetch(`http://127.0.0.1:${port}/api/sim`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ sim }),
  });
  if (!res.ok) throw new Error(`unexpected status ${res.status} from /api/sim`);
  return (await res.json()) as { sim: string; connected: boolean; toolNames: string[] };
}

async function callMcpToolsList(mcpPort: number): Promise<{ name: string }[]> {
  const res = await fetch(`http://127.0.0.1:${mcpPort}/mcp`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/list',
      params: {},
    }),
  });
  if (!res.ok) throw new Error(`unexpected status ${res.status} from MCP HTTP`);
  const body = (await res.json()) as { result?: { tools?: { name: string }[] } };
  return body.result?.tools ?? [];
}

function spawnLauncher(
  appDataDir: string,
  settingsPort: number,
  extraArgs: string[] = []
): ChildProcess {
  return spawn(
    LAUNCHER,
    ['--headless', '--settings-bind', `127.0.0.1:${settingsPort}`, ...extraArgs],
    { env: { ...process.env, APPDATA: appDataDir } }
  );
}

async function killProcess(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.killed) return;
  return new Promise((resolve, reject) => {
    child.once('exit', () => resolve());
    child.once('error', reject);
    child.kill('SIGTERM');
    setTimeout(() => {
      if (child.exitCode === null && !child.killed) {
        child.kill('SIGKILL');
      }
    }, 5000);
  });
}

test.describe('settings UI', () => {
  test('default state shows iRacing and not running', async ({ page }) => {
    const appDataDir = tmpAppDataDir();
    const settingsPort = await getFreePort();
    const child = spawnLauncher(appDataDir, settingsPort);

    try {
      await waitForServer(settingsPort);

      const status = await fetchStatus(settingsPort);
      expect(status.sim).toBe('iracing');
      expect(status.connected).toBe(false);

      await page.goto(`http://127.0.0.1:${settingsPort}/`);
      await expect(page.getByTestId('active-sim')).toHaveText('iRacing');
      await expect(page.getByTestId('connection-status')).toHaveText('Not Running');
    } finally {
      await killProcess(child);
      fs.rmSync(appDataDir, { recursive: true, force: true });
    }
  });

  test('select iRacing via the UI', async ({ page }) => {
    const appDataDir = tmpAppDataDir();
    const settingsPort = await getFreePort();
    // Start from LMU so selecting iRacing is a real change.
    const child = spawnLauncher(appDataDir, settingsPort, ['--sim', 'lmu']);

    try {
      await waitForServer(settingsPort);
      await page.goto(`http://127.0.0.1:${settingsPort}/`);

      await page.getByTestId('select-iracing').click();
      await expect(page.getByTestId('active-sim')).toHaveText('iRacing');
      await expect(page.getByTestId('connection-status')).toHaveText('Not Running');

      const status = await fetchStatus(settingsPort);
      expect(status.sim).toBe('iracing');
      expect(status.connected).toBe(false);
      expect(status.toolNames).toContain('replay_get_state');
    } finally {
      await killProcess(child);
      fs.rmSync(appDataDir, { recursive: true, force: true });
    }
  });

  test('select LMU via the UI', async ({ page }) => {
    const appDataDir = tmpAppDataDir();
    const settingsPort = await getFreePort();
    const child = spawnLauncher(appDataDir, settingsPort);

    try {
      await waitForServer(settingsPort);
      await page.goto(`http://127.0.0.1:${settingsPort}/`);

      await page.getByTestId('select-lmu').click();
      await expect(page.getByTestId('active-sim')).toHaveText('LMU');
      await expect(page.getByTestId('connection-status')).toHaveText('Not Running');

      const status = await fetchStatus(settingsPort);
      expect(status.sim).toBe('lmu');
      expect(status.connected).toBe(false);
      expect(status.toolNames).toContain('get_session_data');
    } finally {
      await killProcess(child);
      fs.rmSync(appDataDir, { recursive: true, force: true });
    }
  });

  test('switching hot-swaps the live MCP server', async ({ page }) => {
    const appDataDir = tmpAppDataDir();
    const settingsPort = await getFreePort();
    const mcpPort = await getFreePort();
    const child = spawnLauncher(appDataDir, settingsPort, [
      '--transport',
      'http',
      '--bind',
      `127.0.0.1:${mcpPort}`,
    ]);

    try {
      await waitForServer(settingsPort);
      await page.goto(`http://127.0.0.1:${settingsPort}/`);

      const initialTools = await callMcpToolsList(mcpPort);
      expect(initialTools.some((t) => t.name === 'replay_get_state')).toBe(true);

      await page.getByTestId('select-lmu').click();
      await expect(page.getByTestId('active-sim')).toHaveText('LMU');

      const lmuTools = await callMcpToolsList(mcpPort);
      expect(lmuTools.some((t) => t.name === 'get_session_data')).toBe(true);
      expect(lmuTools.some((t) => t.name === 'replay_get_state')).toBe(false);

      await page.getByTestId('select-iracing').click();
      await expect(page.getByTestId('active-sim')).toHaveText('iRacing');

      const iracingTools = await callMcpToolsList(mcpPort);
      expect(iracingTools.some((t) => t.name === 'replay_get_state')).toBe(true);
      expect(iracingTools.some((t) => t.name === 'get_session_data')).toBe(false);
    } finally {
      await killProcess(child);
      fs.rmSync(appDataDir, { recursive: true, force: true });
    }
  });

  test('persistence across restart: LMU', async ({ page }) => {
    const appDataDir = tmpAppDataDir();
    const settingsPort = await getFreePort();
    let child = spawnLauncher(appDataDir, settingsPort);

    try {
      await waitForServer(settingsPort);
      await page.goto(`http://127.0.0.1:${settingsPort}/`);
      await page.getByTestId('select-lmu').click();
      await expect(page.getByTestId('active-sim')).toHaveText('LMU');

      await killProcess(child);

      child = spawnLauncher(appDataDir, settingsPort);
      await waitForServer(settingsPort);
      await page.goto(`http://127.0.0.1:${settingsPort}/`);

      await expect(page.getByTestId('active-sim')).toHaveText('LMU');
      const status = await fetchStatus(settingsPort);
      expect(status.sim).toBe('lmu');
      expect(status.connected).toBe(false);
    } finally {
      await killProcess(child);
      fs.rmSync(appDataDir, { recursive: true, force: true });
    }
  });

  test('persistence across restart: iRacing', async ({ page }) => {
    const appDataDir = tmpAppDataDir();
    const settingsPort = await getFreePort();
    let child = spawnLauncher(appDataDir, settingsPort, ['--sim', 'lmu']);

    try {
      await waitForServer(settingsPort);
      await page.goto(`http://127.0.0.1:${settingsPort}/`);
      await page.getByTestId('select-iracing').click();
      await expect(page.getByTestId('active-sim')).toHaveText('iRacing');

      await killProcess(child);

      child = spawnLauncher(appDataDir, settingsPort);
      await waitForServer(settingsPort);
      await page.goto(`http://127.0.0.1:${settingsPort}/`);

      await expect(page.getByTestId('active-sim')).toHaveText('iRacing');
      const status = await fetchStatus(settingsPort);
      expect(status.sim).toBe('iracing');
      expect(status.connected).toBe(false);
    } finally {
      await killProcess(child);
      fs.rmSync(appDataDir, { recursive: true, force: true });
    }
  });
});
