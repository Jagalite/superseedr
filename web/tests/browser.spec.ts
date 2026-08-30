import { expect, test, type Page } from "@playwright/test";

const PRODUCTION_SCREENS = [
  "welcome",
  "normal",
  "help",
  "config",
  "file-browser",
  "rss",
  "journal",
  "peer-management",
  "torrent-management",
  "power-saving",
  "delete-confirm",
] as const;

const SCENARIOS = [
  ["downloading", 3],
  ["seeding", 3],
  ["mixed", 7],
  ["swarm", 3],
  ["missing-pieces", 1],
  ["disk-pressure", 2],
  ["disk-error", 1],
  ["recovery", 2],
] as const;

async function expectReady(page: Page, screen = "normal") {
  const terminal = page.locator("#terminal");
  await expect(page.locator("#status")).toHaveAttribute("data-ready", "true");
  await expect(terminal).toHaveAttribute("data-ready", "true");
  await expect(terminal).toHaveAttribute("data-current-screen", screen);
  await expect.poll(async () => Number(await terminal.getAttribute("data-frame-count"))).toBeGreaterThan(0);
  await expect(terminal.locator("canvas").first()).toBeVisible();
  return terminal;
}

function collectErrors(page: Page) {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  return errors;
}

async function openScreen(page: Page, key: string, screen: string) {
  await page.keyboard.press(key);
  await expect(page.locator("#terminal")).toHaveAttribute("data-current-screen", screen);
}

test("every production screen renders through the bundled browser host", async ({ page }) => {
  const errors = collectErrors(page);
  for (const screen of PRODUCTION_SCREENS) {
    await page.goto(`/?screen=${screen}`);
    await expectReady(page, screen);
  }
  expect(errors).toEqual([]);
});

test("browser starts with the native Superseedr default theme", async ({ page }) => {
  await page.goto("/");
  const terminal = await expectReady(page);

  await expect(terminal).toHaveAttribute("data-current-theme", "Catppuccin Mocha");
});

test("every declarative browser scenario is selectable by URL", async ({ page }) => {
  const errors = collectErrors(page);
  for (const [scenario, torrentCount] of SCENARIOS) {
    await page.goto(`/?scenario=${scenario}`);
    const terminal = await expectReady(page);
    await expect(terminal).toHaveAttribute("data-scenario-name", scenario);
    await expect(terminal).toHaveAttribute("data-torrent-count", String(torrentCount));
  }
  expect(errors).toEqual([]);
});

test("scenario failures, missing pieces, busy swarms, and recovery remain coherent", async ({ page }) => {
  test.setTimeout(30_000);
  const errors = collectErrors(page);

  await page.goto("/?scenario=swarm&screen=peer-management");
  let terminal = await expectReady(page, "peer-management");
  expect(Number(await terminal.getAttribute("data-scenario-max-peers"))).toBeGreaterThanOrEqual(16);
  expect(Number(await terminal.getAttribute("data-scenario-max-peers"))).toBeLessThanOrEqual(20);

  await page.goto("/?scenario=missing-pieces");
  terminal = await expectReady(page);
  await expect(terminal).toHaveAttribute("data-scenario-missing-pieces", "4");
  await expect(terminal).toHaveAttribute("data-scenario-warning", "true");
  await expect(terminal).toHaveAttribute("data-simulated-activity", /missing pieces/);
  await expect(terminal).toHaveAttribute("data-scenario-missing-pieces", "0", { timeout: 5_000 });
  await expect(terminal).toHaveAttribute("data-scenario-recovered", "true");
  await expect(terminal).toHaveAttribute("data-scenario-max-peers", "6");

  await page.goto("/?scenario=disk-pressure");
  terminal = await expectReady(page);
  await expect(terminal).toHaveAttribute("data-scenario-disk-state", "pressure");
  await expect(terminal).toHaveAttribute("data-scenario-warning", "true");
  await expect(terminal).toHaveAttribute("data-scenario-disk-state", "healthy", { timeout: 6_000 });
  await expect(terminal).toHaveAttribute("data-scenario-recovered", "true");

  await page.goto("/?scenario=disk-error&screen=journal");
  terminal = await expectReady(page, "journal");
  await expect(terminal).toHaveAttribute("data-scenario-disk-state", "error");
  await expect(terminal).toHaveAttribute("data-simulated-activity", /disk error/);
  await expect(terminal).toHaveAttribute("data-scenario-disk-state", "recovering", { timeout: 4_000 });
  await expect(terminal).toHaveAttribute("data-scenario-disk-state", "healthy", { timeout: 4_000 });
  await expect(terminal).toHaveAttribute("data-scenario-recovered", "true");

  await page.goto("/?scenario=recovery");
  terminal = await expectReady(page);
  await expect(terminal).toHaveAttribute("data-scenario-warning", "true");
  await expect(terminal).toHaveAttribute("data-scenario-missing-pieces", "0", { timeout: 5_000 });
  await expect(terminal).toHaveAttribute("data-scenario-disk-state", "healthy", { timeout: 5_000 });
  await expect(terminal).toHaveAttribute("data-scenario-recovered", "true");
  await expect(terminal).toHaveAttribute("data-scenario-warning", "false");
  expect(errors).toEqual([]);
});

test("scenario-created torrents retain shared pause resume and delete controls", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/?scenario=downloading");
  const terminal = await expectReady(page);
  await terminal.click();
  await expect(terminal).toHaveAttribute("data-torrent-count", "3");

  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "true");
  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "false");
  await openScreen(page, "d", "delete-confirm");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "2");
  expect(errors).toEqual([]);
});

test("browser input reaches production screen and deeper reducers", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();

  await openScreen(page, "m", "help");
  await page.keyboard.press("q");
  await openScreen(page, "Shift+J", "journal");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("q");
  await openScreen(page, "Shift+P", "peer-management");
  await page.keyboard.press("Enter");
  await page.keyboard.press("q");
  await openScreen(page, "z", "power-saving");
  await page.keyboard.press("z");
  await openScreen(page, "c", "config");
  await page.keyboard.press("p");
  await page.keyboard.press("q");
  await openScreen(page, "r", "rss");
  await page.keyboard.press("/");
  await page.keyboard.type("Signal Garden");
  await page.keyboard.press("Enter");
  for (const _character of "Signal Garden") await page.keyboard.press("Backspace");
  await page.keyboard.press("Enter");
  await page.keyboard.press("q");
  await openScreen(page, "a", "file-browser");
  await page.keyboard.press("/");
  await page.keyboard.type("incoming");
  await page.keyboard.press("Enter");
  await expect(terminal).toHaveAttribute("data-current-screen", "file-browser");

  await page.goto("/");
  await expectReady(page);
  await terminal.click();

  await openScreen(page, "Shift+M", "torrent-management");
  await page.keyboard.press("Space");
  await page.keyboard.press("p");
  await page.keyboard.press("Shift+Y");
  await page.keyboard.press("Enter");
  await page.keyboard.press("q");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-last-key-handled", "true");
  expect(errors).toEqual([]);
});

test("file browser parent navigation remains live without a Tokio runtime", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await openScreen(page, "a", "file-browser");
  const frameBefore = Number(await terminal.getAttribute("data-frame-count"));

  await page.keyboard.press("ArrowLeft");

  await expect(terminal).toHaveAttribute("data-current-screen", "file-browser");
  await expect.poll(async () => Number(await terminal.getAttribute("data-frame-count"))).toBeGreaterThan(frameBefore);
  expect(errors).toEqual([]);
});

test("configuration path selection opens the virtual browser and applies the setting", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await expect(terminal).toHaveAttribute("data-default-download-folder", "");
  await openScreen(page, "c", "config");
  for (let index = 0; index < 2; index += 1) await page.keyboard.press("ArrowDown");

  await page.keyboard.press("Space");

  await expect(terminal).toHaveAttribute("data-current-screen", "file-browser");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-current-screen", "config");
  await expect(terminal).toHaveAttribute("data-default-download-folder", ".");
  expect(errors).toEqual([]);
});

test("mocked torrent metadata confirms through the production file-browser handler", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await expect(terminal).toHaveAttribute("data-torrent-count", "7");
  await openScreen(page, "a", "file-browser");

  await page.keyboard.press("Shift+Y");

  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "8");
  expect(errors).toEqual([]);
});

test("paste pause resume and confirmed deletion preserve browser reducer state", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();

  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "true");
  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "false");

  await expect(terminal).toHaveAttribute("data-torrent-count", "7");
  await page.evaluate(() => {
    const data = new DataTransfer();
    data.setData("text", "magnet:?xt=urn:btih:c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1");
    document.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }));
  });
  await expect(terminal).toHaveAttribute("data-torrent-count", "8");

  await openScreen(page, "d", "delete-confirm");
  await expect(terminal).toHaveAttribute("data-torrent-count", "8");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "7");
  expect(errors).toEqual([]);
});

test("dynamic torrent crosses the complete simulated lifecycle with coherent metrics", async ({ page }) => {
  test.setTimeout(30_000);
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  const visualizationStart = Number(await terminal.getAttribute("data-visualization-phase"));
  const simulationTicksStart = Number(await terminal.getAttribute("data-simulation-tick-count"));

  await page.evaluate(() => {
    const data = new DataTransfer();
    data.setData("text", "magnet:?xt=urn:btih:d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2");
    document.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }));
  });
  await expect(terminal).toHaveAttribute("data-torrent-count", "8");

  const phases = new Set<string>();
  const stalls = new Set<string>();
  let previousBytes = 0;
  let sawActiveDownload = false;
  for (let sample = 0; sample < 240; sample += 1) {
    const phase = (await terminal.getAttribute("data-simulated-phase")) ?? "";
    const stall = (await terminal.getAttribute("data-simulated-stall")) ?? "";
    const bytes = Number(await terminal.getAttribute("data-simulated-bytes-written"));
    const total = Number(await terminal.getAttribute("data-simulated-total-size"));
    const downloadBps = Number(await terminal.getAttribute("data-simulated-download-bps"));
    const peers = Number(await terminal.getAttribute("data-simulated-peers"));
    phases.add(phase);
    if (stall !== "") stalls.add(stall);
    expect(bytes).toBeGreaterThanOrEqual(previousBytes);
    if (total > 0) expect(bytes).toBeLessThanOrEqual(total);
    previousBytes = bytes;
    if (phase === "downloading" && downloadBps > 0) {
      sawActiveDownload = true;
      expect(peers).toBeGreaterThan(0);
    }
    if (phase === "seeding") break;
    await page.waitForTimeout(50);
  }

  for (const phase of ["metadata", "peers", "downloading", "checking", "seeding"]) {
    expect(phases.has(phase)).toBe(true);
  }
  expect(stalls.has("peer")).toBe(true);
  expect(stalls.has("disk")).toBe(true);
  expect(sawActiveDownload).toBe(true);
  await expect(terminal).toHaveAttribute("data-simulated-complete", "true");
  expect(Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBe(
    Number(await terminal.getAttribute("data-simulated-total-size")),
  );
  expect(Number(await terminal.getAttribute("data-simulated-upload-bps"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-visualization-phase"))).toBeGreaterThan(visualizationStart);
  expect(Number(await terminal.getAttribute("data-simulation-tick-count"))).toBeGreaterThan(simulationTicksStart);
  expect(Number(await terminal.getAttribute("data-network-history-samples"))).toBeGreaterThanOrEqual(120);
  expect(Number(await terminal.getAttribute("data-activity-history-samples"))).toBeGreaterThanOrEqual(120);
  expect(Number(await terminal.getAttribute("data-peer-connected-events"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-peer-connected-events"))).toBeLessThanOrEqual(40);
  expect(Number(await terminal.getAttribute("data-peer-discovered-events"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-recent-file-activity"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-swarm-availability-samples"))).toBeGreaterThan(0);
  await expect(terminal).toHaveAttribute("data-dht-wave-initialized", "true");
  expect(errors).toEqual([]);
});

test("pause resume and delete control the selected dynamic torrent", async ({ page }) => {
  test.setTimeout(20_000);
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();

  await page.evaluate(() => {
    const data = new DataTransfer();
    data.setData("text", "magnet:?xt=urn:btih:e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3");
    document.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }));
  });
  await expect(terminal).toHaveAttribute("data-torrent-count", "8");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading", { timeout: 5_000 });
  await expect.poll(async () => Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBeGreaterThan(0);

  await page.keyboard.press("End");
  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "true");
  const pausedBytes = Number(await terminal.getAttribute("data-simulated-bytes-written"));
  await page.waitForTimeout(500);
  expect(Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBe(pausedBytes);
  expect(Number(await terminal.getAttribute("data-simulated-download-bps"))).toBe(0);

  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "false");
  await expect.poll(async () => Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBeGreaterThan(
    pausedBytes,
  );

  await openScreen(page, "d", "delete-confirm");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "7");
  expect(errors).toEqual([]);
});

test("slow terminal writes do not throttle simulation or visualization time", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/?writerDelayMs=250");
  const terminal = await expectReady(page);
  const ticksBefore = Number(await terminal.getAttribute("data-simulation-tick-count"));
  const framesBefore = Number(await terminal.getAttribute("data-frame-count"));
  const phaseBefore = Number(await terminal.getAttribute("data-visualization-phase"));

  await page.waitForTimeout(1_100);

  const simulationTicks =
    Number(await terminal.getAttribute("data-simulation-tick-count")) - ticksBefore;
  const writtenFrames = Number(await terminal.getAttribute("data-frame-count")) - framesBefore;
  expect(simulationTicks).toBeGreaterThanOrEqual(30);
  expect(simulationTicks).toBeGreaterThan(writtenFrames * 3);
  expect(Number(await terminal.getAttribute("data-visualization-phase"))).toBeGreaterThan(phaseBefore);
  await expect(terminal).toHaveAttribute("data-max-concurrent-writes", "1");
  expect(errors).toEqual([]);
});

test("resize zoom animation serialization and page lifecycle remain bounded", async ({ page, context }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  const initialColumns = Number(await terminal.getAttribute("data-cols"));

  await page.setViewportSize({ width: 900, height: 600 });
  await expect.poll(async () => Number(await terminal.getAttribute("data-cols"))).toBeLessThan(initialColumns);
  await page.setViewportSize({ width: 1280, height: 800 });
  await expect.poll(async () => Number(await terminal.getAttribute("data-cols"))).toBeGreaterThan(100);

  const fitBeforeZoom = Number(await terminal.getAttribute("data-fit-count"));
  const devtools = await context.newCDPSession(page);
  await devtools.send("Emulation.setDeviceMetricsOverride", {
    width: 1280,
    height: 800,
    deviceScaleFactor: 2,
    mobile: false,
  });
  await expect(terminal).toHaveAttribute("data-device-pixel-ratio", "2");
  await expect.poll(async () => Number(await terminal.getAttribute("data-fit-count"))).toBeGreaterThan(fitBeforeZoom);

  const animationStart = Number(await terminal.getAttribute("data-frame-count"));
  await page.waitForTimeout(1_100);
  const animationEnd = Number(await terminal.getAttribute("data-frame-count"));
  expect(animationEnd - animationStart).toBeGreaterThanOrEqual(38);
  await expect(terminal).toHaveAttribute("data-max-concurrent-writes", "1");

  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { configurable: true, value: "hidden" });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await page.waitForTimeout(100);
  const backgroundStart = Number(await terminal.getAttribute("data-frame-count"));
  await page.waitForTimeout(300);
  expect(Number(await terminal.getAttribute("data-frame-count"))).toBe(backgroundStart);
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { configurable: true, value: "visible" });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expect.poll(async () => Number(await terminal.getAttribute("data-frame-count"))).toBeGreaterThan(backgroundStart);

  await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent("pagehide")));
  await page.waitForTimeout(100);
  const hiddenStart = Number(await terminal.getAttribute("data-frame-count"));
  await page.waitForTimeout(300);
  expect(Number(await terminal.getAttribute("data-frame-count"))).toBe(hiddenStart);
  await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent("pageshow")));
  await expect.poll(async () => Number(await terminal.getAttribute("data-frame-count"))).toBeGreaterThan(hiddenStart);

  await devtools.send("Emulation.clearDeviceMetricsOverride");
  expect(errors).toEqual([]);
});
