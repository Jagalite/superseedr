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
  await expect(terminal).toHaveAttribute("data-target-fps", "60");
  await expect(terminal).toHaveAttribute("data-fps-label", "60 fps");
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
  expect(Number(await terminal.getAttribute("data-scenario-peer-rate-variants"))).toBeGreaterThanOrEqual(8);
  expect(Number(await terminal.getAttribute("data-scenario-availability-levels"))).toBeGreaterThanOrEqual(3);
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-scenario-piece-acquisitions")))
    .toBeGreaterThan(0);

  await page.goto("/?scenario=seeding");
  terminal = await expectReady(page);
  expect(Number(await terminal.getAttribute("data-scenario-peer-rate-variants"))).toBeGreaterThanOrEqual(3);
  expect(Number(await terminal.getAttribute("data-scenario-availability-levels"))).toBeGreaterThanOrEqual(3);

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
  await expect(terminal).toHaveAttribute("data-simulated-peers", "0");
  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "false");
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-simulated-peers")))
    .toBeGreaterThan(0);
  await openScreen(page, "d", "delete-confirm");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "2");
  expect(errors).toEqual([]);
});

test("browser telemetry keeps production units counters and transport semantics", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/?scenario=downloading");
  const terminal = await expectReady(page);

  await expect
    .poll(async () => Number(await terminal.getAttribute("data-simulated-bytes-downloaded-tick")))
    .toBeGreaterThan(0);
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-simulated-eta-seconds")))
    .toBeGreaterThan(0);
  const announce = Number(await terminal.getAttribute("data-simulated-announce-seconds"));
  expect(announce).toBeGreaterThan(0);
  expect(announce).toBeLessThanOrEqual(30 * 60);

  const peers = Number(await terminal.getAttribute("data-simulated-peers"));
  const tcpPeers = Number(await terminal.getAttribute("data-simulated-tcp-peers"));
  const utpPeers = Number(await terminal.getAttribute("data-simulated-utp-peers"));
  const beneficialPeers = Number(await terminal.getAttribute("data-simulated-beneficial-peers"));
  expect(tcpPeers).toBeGreaterThan(0);
  expect(utpPeers).toBeGreaterThan(0);
  expect(tcpPeers + utpPeers).toBe(peers);
  expect(beneficialPeers).toBeGreaterThan(0);
  expect(beneficialPeers).toBeLessThanOrEqual(peers);

  await expect
    .poll(async () => Number(await terminal.getAttribute("data-blocks-received-events")))
    .toBeGreaterThan(0);
  await expect.poll(async () => Number(await terminal.getAttribute("data-read-iops"))).toBeGreaterThan(1);
  await expect.poll(async () => Number(await terminal.getAttribute("data-write-iops"))).toBeGreaterThan(1);
  expect(Number(await terminal.getAttribute("data-disk-read-latency-micros"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-disk-write-latency-micros"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-recv-to-write-latency-micros"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-recent-file-download-activity"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-recent-file-upload-activity"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-tracked-peers"))).toBeGreaterThanOrEqual(peers);
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
  await expect(terminal).toHaveAttribute("data-torrent-preview-state", "ready");
  await expect(terminal).toHaveAttribute("data-torrent-preview-name", "Incoming Demo Set");
  await expect(terminal).toHaveAttribute("data-torrent-preview-file-count", "3");

  await page.keyboard.press("Shift+Y");

  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "8");
  expect(errors).toEqual([]);
});

test("seeding swarm peers churn through upload bursts and no-recipient lulls", async ({ page }) => {
  test.setTimeout(20_000);
  const errors = collectErrors(page);
  await page.goto("/?scenario=seeding");
  const terminal = await expectReady(page);
  await terminal.click();
  await page.keyboard.press("s");
  const initialConnectedEvents = Number(await terminal.getAttribute("data-peer-connected-events"));
  const initialDisconnectedEvents = Number(await terminal.getAttribute("data-peer-disconnected-events"));
  const peerCounts = new Set<number>();
  const positiveUploadRates = new Set<number>();
  let sawNoUploadRecipients = false;
  let sawAverageDecayDuringLull = false;
  let previousUploadBps: number | undefined;

  for (let sample = 0; sample < 80; sample += 1) {
    const peers = Number(await terminal.getAttribute("data-simulated-peers"));
    const recipients = Number(
      await terminal.getAttribute("data-simulated-upload-recipients"),
    );
    const uploadBps = Number(await terminal.getAttribute("data-simulated-upload-bps"));
    peerCounts.add(peers);
    if (recipients === 0 && peers > 0) {
      sawNoUploadRecipients = true;
      if (previousUploadBps !== undefined && uploadBps > 0 && uploadBps < previousUploadBps) {
        sawAverageDecayDuringLull = true;
      }
    }
    if (uploadBps > 0) positiveUploadRates.add(uploadBps);
    previousUploadBps = uploadBps;
    await page.waitForTimeout(100);
  }

  expect(peerCounts.size).toBeGreaterThan(1);
  expect(positiveUploadRates.size).toBeGreaterThan(2);
  expect(sawNoUploadRecipients).toBe(true);
  expect(sawAverageDecayDuringLull).toBe(true);
  expect(Number(await terminal.getAttribute("data-peer-connected-events"))).toBeGreaterThan(
    initialConnectedEvents,
  );
  expect(Number(await terminal.getAttribute("data-peer-disconnected-events"))).toBeGreaterThan(
    initialDisconnectedEvents,
  );
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
  test.setTimeout(45_000);
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
  let previousDownloadBps: number | undefined;
  let largestDownloadRateStep = 0;
  let sawActiveDownload = false;
  let sawDownloadAverageDuringPeerLull = false;
  const checkingDownloadRates: number[] = [];
  const checkingUploadRates: number[] = [];
  for (let sample = 0; sample < 360; sample += 1) {
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
      sawDownloadAverageDuringPeerLull ||= peers === 0;
      if (previousDownloadBps !== undefined) {
        largestDownloadRateStep = Math.max(
          largestDownloadRateStep,
          Math.abs(downloadBps - previousDownloadBps),
        );
      }
      previousDownloadBps = downloadBps;
    }
    if (phase === "checking") {
      expect(bytes).toBe(total);
      checkingDownloadRates.push(downloadBps);
      checkingUploadRates.push(
        Number(await terminal.getAttribute("data-simulated-upload-bps")),
      );
    }
    if (phase === "seeding") break;
    await page.waitForTimeout(50);
  }

  for (const phase of ["metadata", "peers", "downloading", "checking", "seeding"]) {
    expect(phases.has(phase), `missing ${phase}; observed ${[...phases].join(", ")}`).toBe(true);
  }
  expect(stalls.has("peer")).toBe(true);
  expect(stalls.has("disk")).toBe(true);
  expect(sawActiveDownload).toBe(true);
  expect(sawDownloadAverageDuringPeerLull).toBe(true);
  expect(checkingDownloadRates.length).toBeGreaterThan(1);
  expect(checkingUploadRates.length).toBeGreaterThan(1);
  expect(checkingDownloadRates.at(-1)).toBeLessThan(checkingDownloadRates[0]);
  expect(checkingUploadRates.at(-1)).toBeLessThan(checkingUploadRates[0]);
  expect(checkingDownloadRates.at(-1)).toBeGreaterThan(0);
  expect(checkingUploadRates.at(-1)).toBeGreaterThan(0);
  // The largest transition is the expected cold-start ramp from a zero native-style EMA.
  expect(largestDownloadRateStep).toBeLessThan(4 * 1024 * 1024);
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

  const simulatedHash = await terminal.getAttribute("data-simulated-torrent-hash");
  expect(simulatedHash).not.toBe("");
  await page.keyboard.press("s");
  await expect(terminal).toHaveAttribute("data-torrent-sort-column", "name");
  await expect(terminal).toHaveAttribute("data-torrent-sort-pinned", "true");
  await page.keyboard.press("Home");
  for (let index = 0; index < 8; index += 1) {
    const selectedHash = await terminal.getAttribute("data-selected-torrent-hash");
    if (selectedHash === simulatedHash) break;
    await page.keyboard.press("ArrowDown");
    await expect
      .poll(async () => terminal.getAttribute("data-selected-torrent-hash"))
      .not.toBe(selectedHash);
  }
  await expect(terminal).toHaveAttribute("data-selected-torrent-hash", simulatedHash ?? "");
  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-simulated-torrent-paused", "true");
  const pausedBytes = Number(await terminal.getAttribute("data-simulated-bytes-written"));
  await page.waitForTimeout(500);
  expect(Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBe(pausedBytes);
  expect(Number(await terminal.getAttribute("data-simulated-download-bps"))).toBe(0);

  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-simulated-torrent-paused", "false");
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

test("torrent progress publishes at the display-frame cadence", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await page.evaluate(() => {
    const data = new DataTransfer();
    data.setData("text", "magnet:?xt=urn:btih:f4f4f4f4f4f4f4f4f4f4f4f4f4f4f4f4f4f4f4f4");
    document.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }));
  });
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading", {
    timeout: 5_000,
  });

  const frameSamples = await terminal.evaluate(
    (element) =>
      new Promise<{ progress: number; rates: number; fpsLabels: string[] }>((resolve) => {
        const progressValues = new Set<string>();
        const rateValues = new Set<string>();
        const fpsLabels = new Set<string>();
        const startedAt = performance.now();
        const sample = (now: number): void => {
          progressValues.add(element.dataset.simulatedBytesWritten ?? "");
          rateValues.add(element.dataset.simulatedDownloadBps ?? "");
          fpsLabels.add(element.dataset.fpsLabel ?? "");
          if (now - startedAt >= 400) {
            resolve({
              progress: progressValues.size,
              rates: rateValues.size,
              fpsLabels: [...fpsLabels],
            });
          } else requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
      }),
  );

  expect(frameSamples.progress).toBeGreaterThanOrEqual(12);
  expect(frameSamples.rates).toBeGreaterThanOrEqual(12);
  expect(frameSamples.fpsLabels).toEqual(["60 fps"]);
  expect(errors).toEqual([]);
});

test("torrent autosort follows download and upload activity", async ({ page }) => {
  const errors = collectErrors(page);
  const orderedRates = async (attribute: string): Promise<number[]> =>
    ((await page.locator("#terminal").getAttribute(attribute)) ?? "")
      .split(",")
      .filter(Boolean)
      .map(Number);
  const isDescending = (rates: number[]): boolean =>
    rates.length > 1 &&
    rates.some((rate, index) => index > 0 && rates[index - 1] > rate) &&
    rates.every((rate, index) => index === 0 || rates[index - 1] >= rate);

  await page.goto("/?scenario=downloading");
  let terminal = await expectReady(page);
  await expect(terminal).toHaveAttribute("data-torrent-sort-column", "down");
  await expect(terminal).toHaveAttribute("data-torrent-sort-pinned", "false");
  await expect
    .poll(async () => isDescending(await orderedRates("data-ordered-torrent-download-rates")))
    .toBe(true);

  await page.goto("/?scenario=seeding");
  terminal = await expectReady(page);
  await expect(terminal).toHaveAttribute("data-torrent-sort-column", "up");
  await expect(terminal).toHaveAttribute("data-torrent-sort-pinned", "false");
  await expect
    .poll(async () => isDescending(await orderedRates("data-ordered-torrent-upload-rates")))
    .toBe(true);
  expect(errors).toEqual([]);
});

test("terminal consumes wheel and touch scrolling", async ({ page }) => {
  const errors = collectErrors(page);
  await page.setViewportSize({ width: 800, height: 400 });
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.hover();
  const scrollBefore = await page.evaluate(() => window.scrollY);

  await page.mouse.wheel(0, 600);
  await page.waitForTimeout(100);

  expect(await page.evaluate(() => window.scrollY)).toBe(scrollBefore);
  expect(
    await terminal.evaluate((element) => {
      const event = new WheelEvent("wheel", { bubbles: true, cancelable: true, deltaY: 100 });
      element.dispatchEvent(event);
      return event.defaultPrevented;
    }),
  ).toBe(true);
  expect(Number(await terminal.getAttribute("data-scroll-blocked-count"))).toBeGreaterThan(0);
  expect(await terminal.evaluate((element) => getComputedStyle(element).touchAction)).toBe("none");
  expect(errors).toEqual([]);
});

test("font readiness and delayed layout settlement produce the initial terminal fit", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/?fontReadyDelayMs=200&layoutSettleDelayMs=200");
  const terminal = page.locator("#terminal");
  await terminal.evaluate((element) => {
    element.style.width = "720px";
  });

  await expectReady(page);
  await expect(terminal).toHaveAttribute("data-fonts-ready", "true");
  await expect(terminal).toHaveAttribute("data-initial-fit-settled", "true");
  expect(Number(await terminal.getAttribute("data-fit-count"))).toBeGreaterThanOrEqual(2);
  expect(Number(await terminal.getAttribute("data-cols"))).toBeLessThan(100);
  expect(errors).toEqual([]);
});

test("terminal container-only resize triggers immediate and settled fits", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  const initialColumns = Number(await terminal.getAttribute("data-cols"));
  const initialViewportWidth = await page.evaluate(() => window.innerWidth);
  const initialFitCount = Number(await terminal.getAttribute("data-fit-count"));
  const initialObserverCount = Number(await terminal.getAttribute("data-resize-observer-count"));

  await terminal.evaluate((element) => {
    element.style.width = "640px";
  });

  await expect.poll(async () => Number(await terminal.getAttribute("data-cols"))).toBeLessThan(initialColumns);
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-fit-count")))
    .toBeGreaterThanOrEqual(initialFitCount + 2);
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-resize-observer-count")))
    .toBeGreaterThan(initialObserverCount);
  expect(await page.evaluate(() => window.innerWidth)).toBe(initialViewportWidth);
  expect(errors).toEqual([]);
});

test("viewport resize refits through the shared production resize path", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  const initialColumns = Number(await terminal.getAttribute("data-cols"));
  const initialFitCount = Number(await terminal.getAttribute("data-fit-count"));

  await page.setViewportSize({ width: 900, height: 600 });
  await expect.poll(async () => Number(await terminal.getAttribute("data-cols"))).toBeLessThan(initialColumns);
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-fit-count")))
    .toBeGreaterThanOrEqual(initialFitCount + 2);
  await page.setViewportSize({ width: 1280, height: 800 });
  await expect.poll(async () => Number(await terminal.getAttribute("data-cols"))).toBeGreaterThan(100);
  expect(errors).toEqual([]);
});

test("browser zoom-in keeps the terminal fitted inside the viewport", async ({ page, context }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  const initialColumns = Number(await terminal.getAttribute("data-cols"));
  const initialRows = Number(await terminal.getAttribute("data-rows"));
  const devtools = await context.newCDPSession(page);

  await devtools.send("Emulation.setDeviceMetricsOverride", {
    width: 640,
    height: 400,
    deviceScaleFactor: 2,
    mobile: false,
  });

  await expect(terminal).toHaveAttribute("data-device-pixel-ratio", "2");
  await expect.poll(async () => Number(await terminal.getAttribute("data-cols"))).toBeLessThan(initialColumns);
  await expect.poll(async () => Number(await terminal.getAttribute("data-rows"))).toBeLessThan(initialRows);
  await expect
    .poll(() =>
      page.evaluate(() => {
        const frame = document.querySelector<HTMLElement>(".terminal-frame");
        const host = document.querySelector<HTMLElement>("#terminal");
        const canvas = host?.querySelector("canvas");
        if (frame === null || host === null || canvas === null) return false;
        const frameRect = frame.getBoundingClientRect();
        const hostRect = host.getBoundingClientRect();
        const canvasRect = canvas.getBoundingClientRect();
        return (
          document.documentElement.scrollWidth <= window.innerWidth &&
          document.documentElement.scrollHeight <= window.innerHeight &&
          frameRect.bottom <= window.innerHeight + 1 &&
          hostRect.width > 0 &&
          hostRect.height > 0 &&
          canvasRect.right <= hostRect.right + 1 &&
          canvasRect.bottom <= hostRect.bottom + 1
        );
      }),
    )
    .toBe(true);

  await devtools.send("Emulation.setDeviceMetricsOverride", {
    width: 1280,
    height: 800,
    deviceScaleFactor: 1,
    mobile: false,
  });
  await expect.poll(async () => Number(await terminal.getAttribute("data-cols"))).toBeGreaterThan(100);
  await expect.poll(async () => Number(await terminal.getAttribute("data-rows"))).toBeGreaterThan(30);
  await devtools.send("Emulation.clearDeviceMetricsOverride");
  expect(errors).toEqual([]);
});

test("device-pixel-ratio zoom triggers immediate and settled fits", async ({ page, context }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  const canvas = terminal.locator("canvas");
  const fitBeforeZoom = Number(await terminal.getAttribute("data-fit-count"));
  const devtools = await context.newCDPSession(page);
  await devtools.send("Emulation.setDeviceMetricsOverride", {
    width: 1280,
    height: 800,
    deviceScaleFactor: 2,
    mobile: false,
  });

  await expect(terminal).toHaveAttribute("data-device-pixel-ratio", "2");
  await expect(terminal).toHaveAttribute("data-renderer-device-pixel-ratio", "2");
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-fit-count")))
    .toBeGreaterThanOrEqual(fitBeforeZoom + 2);
  await expect
    .poll(() => canvas.evaluate((element) => element.width / element.clientWidth))
    .toBeCloseTo(2, 1);
  await devtools.send("Emulation.clearDeviceMetricsOverride");
  expect(errors).toEqual([]);
});

test("animation serialization and page lifecycle remain bounded", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);

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

  expect(errors).toEqual([]);
});
