# Sim RaceCenter Launcher — End-to-End Tests

This Node/TypeScript project drives the launcher's settings UI in a real browser
and asserts the settings server, simulator hot-swap, and config persistence
behavior on Linux CI. It lives outside the Cargo workspace (`e2e/`) so it can use
Playwright without mixing build systems.

## Run

Requires the launcher binary to be built first:

```sh
cargo build --workspace
cd e2e
npm ci
npx playwright test
```

To use a system Chrome instead of the Playwright-managed Chromium, set
`PLAYWRIGHT_CHROME_BIN`:

```sh
PLAYWRIGHT_CHROME_BIN=/opt/.devin/playwright_browsers/chromium-1097/chrome-linux/chrome npx playwright test
```

The suite spawns the launcher with a fresh `APPDATA` temp directory per test, so
tests do not share config state.
