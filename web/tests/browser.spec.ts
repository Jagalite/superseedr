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
  ["mixed", 15],
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

  await expect(page).toHaveTitle("superseedr interactive demo");
  await expect(page.getByText("Superseedr Web")).toHaveCount(0);
  await expect(page.getByText("superseedr interactive demo", { exact: true })).toBeVisible();
  expect(
    await page
      .getByRole("link", { name: "Open the Superseedr source repository" })
      .evaluate((link) => new URL((link as HTMLAnchorElement).href).pathname),
  ).toBe("/Jagalite/superseedr");
  expect(await page.locator("body").evaluate((element) => getComputedStyle(element).backgroundColor)).toBe(
    "rgb(0, 0, 0)",
  );
  await expect(terminal).toHaveAttribute("data-current-theme", /.+/);
  await expect(terminal).toHaveAttribute("data-font-size", "10");
  await expect(terminal).toHaveAttribute("data-target-fps", "60");
  await expect(terminal).toHaveAttribute("data-fps-label", "60 fps");
  await expect(terminal).toHaveAttribute("data-scenario-paused-count", "0");
  await expect(terminal).toHaveAttribute("data-scenario-deleting-count", "0");
  await expect(page.getByText("Production TUI, simulated session")).toHaveCount(0);
  await expect(page.getByText("Simulated demo — no network or disk activity")).toHaveCount(0);
  await expect(
    page.getByText("Click the terminal to focus. Keyboard and paste input use the production reducers."),
  ).toHaveCount(0);
});

test("terminal stays visually active while preserving interactive element focus", async ({ page }) => {
  await page.goto("/");
  const terminal = await expectReady(page);
  const frame = page.locator(".terminal-frame");
  const repositoryLink = page.getByRole("link", { name: "Open the Superseedr source repository" });

  await expect(terminal).toBeFocused();
  await page.getByText("superseedr interactive demo", { exact: true }).click();
  await expect(terminal).toBeFocused();
  await page.waitForTimeout(50);
  await repositoryLink.focus();
  await expect(repositoryLink).toBeFocused();
  expect(await frame.evaluate((element) => getComputedStyle(element).borderColor)).toBe(
    "rgb(66, 103, 88)",
  );
  await expect(terminal).toHaveAttribute("data-cursor-hidden", "true");
  await expect(terminal).toHaveAttribute("data-input-focus-policy", "automatic");
});

test("coarse-pointer startup stays active without requesting keyboard focus", async ({ page }) => {
  await page.addInitScript(() => {
    const nativeMatchMedia = window.matchMedia.bind(window);
    window.matchMedia = (query: string): MediaQueryList => {
      if (query !== "(pointer: coarse)") return nativeMatchMedia(query);
      return {
        matches: true,
        media: query,
        onchange: null,
        addListener: () => undefined,
        removeListener: () => undefined,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
        dispatchEvent: () => true,
      };
    };
  });
  await page.goto("/");
  const terminal = await expectReady(page);
  const frame = page.locator(".terminal-frame");

  await expect(terminal).not.toBeFocused();
  await expect(terminal).toHaveAttribute("data-input-focus-policy", "tap");
  await expect(terminal).toHaveAttribute("data-cursor-hidden", "true");
  expect(await frame.evaluate((element) => getComputedStyle(element).borderColor)).toBe(
    "rgb(66, 103, 88)",
  );
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

test("default incomplete torrents download concurrently", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);

  await expect
    .poll(async () => {
      const rates = (await terminal.getAttribute("data-ordered-torrent-download-rates")) ?? "";
      return rates
        .split(",")
        .map(Number)
        .filter((rate) => rate > 0).length;
    })
    .toBeGreaterThanOrEqual(8);
  expect(errors).toEqual([]);
});

test("default torrents receive uneven shares of the prioritized three-hundred-megabit link", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  let maximumDownloadRatio = 0;
  let maximumUploadRatio = 0;
  let saturatedCombinedSamples = 0;
  let downloadPrioritySamples = 0;

  for (let sample = 0; sample < 30; sample += 1) {
    const state = await terminal.evaluate((element) => ({
      downloadRates: (element.dataset.orderedTorrentDownloadRates ?? "")
        .split(",")
        .map(Number)
        .filter((rate) => rate > 0),
      uploadRates: (element.dataset.orderedTorrentUploadRates ?? "")
        .split(",")
        .map(Number)
        .filter((rate) => rate > 0),
      totalDownload: Number(element.dataset.totalDownloadBps),
      totalUpload: Number(element.dataset.totalUploadBps),
    }));
    if (state.downloadRates.length > 1) {
      maximumDownloadRatio = Math.max(
        maximumDownloadRatio,
        Math.max(...state.downloadRates) / Math.min(...state.downloadRates),
      );
    }
    if (state.uploadRates.length > 1) {
      maximumUploadRatio = Math.max(
        maximumUploadRatio,
        Math.max(...state.uploadRates) / Math.min(...state.uploadRates),
      );
    }
    const combined = state.totalDownload + state.totalUpload;
    expect(combined).toBeLessThanOrEqual(330_000_000);
    saturatedCombinedSamples += Number(combined >= 240_000_000);
    downloadPrioritySamples += Number(state.totalDownload > state.totalUpload);
    await page.waitForTimeout(100);
  }

  expect(maximumDownloadRatio).toBeGreaterThanOrEqual(8);
  expect(maximumUploadRatio).toBeGreaterThanOrEqual(8);
  expect(saturatedCombinedSamples).toBeGreaterThan(15);
  expect(downloadPrioritySamples).toBeGreaterThan(15);
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
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-scenario-peer-rate-variants")), {
      timeout: 10_000,
    })
    .toBeGreaterThanOrEqual(3);
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

test("committed composition text reaches the production input reducer", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await openScreen(page, "r", "rss");
  await page.keyboard.press("Tab");
  await page.keyboard.press("a");

  await page.evaluate(() => {
    document.dispatchEvent(
      new CompositionEvent("compositionend", {
        bubbles: true,
        data: "https://composition.invalid/feed.xml",
      }),
    );
  });
  await expect(terminal).toHaveAttribute(
    "data-last-composition",
    "https://composition.invalid/feed.xml",
  );
  await page.keyboard.press("Enter");
  await expect(terminal).toHaveAttribute("data-rss-feed-count", "2");
  expect(errors).toEqual([]);
});

test("focused terminal preserves the browser paste shortcut", async ({ page }) => {
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.focus();

  const shortcut = await terminal.evaluate((element) => {
    const event = new KeyboardEvent("keydown", {
      key: "v",
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    element.dispatchEvent(event);
    return { defaultPrevented: event.defaultPrevented };
  });

  expect(shortcut.defaultPrevented).toBe(false);
});

test("AltGraph printable input reaches production text reducers without Ctrl or Alt", async ({ page }) => {
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.focus();
  await page.keyboard.press("/");

  await terminal.evaluate((element) => {
    const event = new KeyboardEvent("keydown", {
      key: "@",
      ctrlKey: true,
      altKey: true,
      bubbles: true,
      cancelable: true,
    });
    Object.defineProperty(event, "getModifierState", {
      value: (modifier: string) => modifier === "AltGraph",
    });
    element.dispatchEvent(event);
  });

  await expect(terminal).toHaveAttribute("data-last-key", "@");
  await expect(terminal).toHaveAttribute("data-last-key-handled", "true");
});

test("focused terminal preserves browser zoom shortcuts", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();

  const prevented = await page.evaluate(() =>
    ["-", "+", "=", "0"].map((key) => {
      const event = new KeyboardEvent("keydown", {
        key,
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      });
      document.dispatchEvent(event);
      return event.defaultPrevented;
    }),
  );
  expect(prevented).toEqual([false, false, false, false]);
  expect(errors).toEqual([]);
});

test("refresh-rate controls throttle browser publication and rendering", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();

  await page.keyboard.press("[");
  await expect(terminal).toHaveAttribute("data-target-fps", "30");
  const framesBefore = Number(await terminal.getAttribute("data-frame-count"));
  await page.waitForTimeout(1_200);
  const renderedFrames = Number(await terminal.getAttribute("data-frame-count")) - framesBefore;
  expect(renderedFrames).toBeGreaterThanOrEqual(20);
  expect(renderedFrames).toBeLessThanOrEqual(42);

  await page.keyboard.press("]");
  await expect(terminal).toHaveAttribute("data-target-fps", "60");
  expect(errors).toEqual([]);
});

test("RSS configuration sync and preview download effects remain interactive", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await openScreen(page, "r", "rss");

  await expect(terminal).toHaveAttribute("data-rss-feed-count", "1");
  await expect(terminal).toHaveAttribute("data-rss-enabled-feed-count", "1");
  await page.keyboard.press("s");
  await expect(terminal).toHaveAttribute("data-rss-last-sync-at", "2026-08-30T12:05:00Z");

  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  await expect(terminal).toHaveAttribute("data-rss-history-count", "1");
  await expect(terminal).toHaveAttribute("data-rss-downloaded-preview-count", "1");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading", {
    timeout: 5_000,
  });
  const bytesBeforeDuplicate = Number(
    await terminal.getAttribute("data-simulated-bytes-written"),
  );
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading");
  expect(Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBeGreaterThanOrEqual(
    bytesBeforeDuplicate,
  );

  await page.keyboard.press("Tab");
  await page.keyboard.press("Space");
  await expect(terminal).toHaveAttribute("data-rss-enabled-feed-count", "0");

  await page.keyboard.press("a");
  await page.keyboard.type("https://second.invalid/feed.xml");
  await page.keyboard.press("Enter");
  await expect(terminal).toHaveAttribute("data-rss-feed-count", "2");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Shift+D");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-rss-feed-count", "1");
  expect(errors).toEqual([]);
});

test("invalid magnets are rejected and base32 identity remains stable", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  const paste = async (text: string): Promise<void> => {
    await page.evaluate((value) => {
      const data = new DataTransfer();
      data.setData("text", value);
      document.dispatchEvent(
        new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }),
      );
    }, text);
  };

  await paste("magnet:not-a-link");
  await expect(terminal).toHaveAttribute(
    "data-system-error",
    "Pasted content is not a valid magnet with a supported info hash.",
  );
  await expect(terminal).toHaveAttribute("data-torrent-count", "15");

  const base32 = "magnet:?XT=URN:BTIH:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&dn=Orbit%20Archive";
  await paste(base32);
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  await expect(terminal).toHaveAttribute(
    "data-simulated-torrent-hash",
    "0000000000000000000000000000000000000000",
  );
  await expect(terminal).toHaveAttribute("data-system-error", "");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading", { timeout: 5_000 });
  const bytesBeforeDuplicate = Number(
    await terminal.getAttribute("data-simulated-bytes-written"),
  );
  await paste(base32);
  await page.waitForTimeout(500);
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading");
  expect(Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBeGreaterThanOrEqual(
    bytesBeforeDuplicate,
  );
  expect(errors).toEqual([]);
});

test("file browser parent navigation remains live without an asynchronous runtime", async ({ page }) => {
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
  await page.keyboard.press("q");
  await openScreen(page, "a", "file-browser");
  await expect(terminal).toHaveAttribute("data-torrent-preview-state", "ready");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  expect(errors).toEqual([]);
});

test("configuration exposes the browser-owned network interface inventory", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await expect(terminal).toHaveAttribute("data-browser-network-interface-count", "1");
  await openScreen(page, "c", "config");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Shift+R");
  await expect(terminal).toHaveAttribute("data-browser-network-interface-count", "1");
  expect(errors).toEqual([]);
});

test("configuration transfer limits constrain the simulated browser runtime", async ({ page }) => {
  test.setTimeout(20_000);
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await openScreen(page, "c", "config");
  for (let index = 0; index < 5; index += 1) await page.keyboard.press("ArrowDown");

  await page.keyboard.press("Space");
  await page.keyboard.type("25 Mbps");
  await page.keyboard.press("Y");
  await expect(terminal).toHaveAttribute("data-effective-download-limit-bps", "25000000");

  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Space");
  await page.keyboard.type("10 Mbps");
  await page.keyboard.press("Y");
  await expect(terminal).toHaveAttribute("data-configured-upload-limit-bps", "10000000");

  await page.keyboard.press("q");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  const before = await terminal.evaluate((element) => ({
    downloaded: Number((element as HTMLElement).dataset.aggregateSessionDownloaded),
    elapsed: Number((element as HTMLElement).dataset.simulationElapsedSeconds),
    uploaded: Number((element as HTMLElement).dataset.aggregateSessionUploaded),
  }));
  await page.waitForTimeout(8_000);
  const after = await terminal.evaluate((element) => ({
    downloaded: Number((element as HTMLElement).dataset.aggregateSessionDownloaded),
    elapsed: Number((element as HTMLElement).dataset.simulationElapsedSeconds),
    uploaded: Number((element as HTMLElement).dataset.aggregateSessionUploaded),
  }));
  const elapsedSeconds = after.elapsed - before.elapsed;
  const downloadedBits = (after.downloaded - before.downloaded) * 8;
  const uploadedBits = (after.uploaded - before.uploaded) * 8;
  expect(downloadedBits).toBeGreaterThan(0);
  expect(downloadedBits).toBeLessThanOrEqual(25_000_000 * elapsedSeconds);
  expect(uploadedBits).toBeLessThanOrEqual(10_000_000 * elapsedSeconds);
  expect(errors).toEqual([]);
});

test("mocked torrent metadata confirms through the production file-browser handler", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();
  await expect(terminal).toHaveAttribute("data-torrent-count", "15");
  await openScreen(page, "a", "file-browser");
  await expect(terminal).toHaveAttribute("data-torrent-preview-state", "ready");
  await expect(terminal).toHaveAttribute("data-torrent-preview-name", "Incoming Demo Set");
  await expect(terminal).toHaveAttribute("data-torrent-preview-file-count", "3");

  await page.keyboard.press("Shift+Y");

  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  await expect(terminal).toHaveAttribute("data-simulated-torrent-name", "Incoming Demo Set");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading", {
    timeout: 5_000,
  });
  const bytesBeforeDuplicate = Number(
    await terminal.getAttribute("data-simulated-bytes-written"),
  );
  await openScreen(page, "a", "file-browser");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading");
  expect(Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBeGreaterThanOrEqual(
    bytesBeforeDuplicate,
  );
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
  let activeRecipientSamples = 0;
  let previousUploadBps: number | undefined;

  for (let sample = 0; sample < 80; sample += 1) {
    const peers = Number(await terminal.getAttribute("data-simulated-peers"));
    const recipients = Number(
      await terminal.getAttribute("data-simulated-upload-recipients"),
    );
    const uploadBps = Number(await terminal.getAttribute("data-simulated-upload-bps"));
    activeRecipientSamples += Number(recipients > 0);
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
  expect(activeRecipientSamples).toBeGreaterThanOrEqual(4);
  expect(activeRecipientSamples).toBeLessThanOrEqual(24);
  expect(Number(await terminal.getAttribute("data-peer-connected-events"))).toBeGreaterThan(
    initialConnectedEvents,
  );
  expect(Number(await terminal.getAttribute("data-peer-disconnected-events"))).toBeGreaterThan(
    initialDisconnectedEvents,
  );
  expect(errors).toEqual([]);
});

test("new seeding peers begin at zero progress within the shared upload link", async ({ page }) => {
  await page.goto("/?scenario=seeding&screen=peer-management");
  const terminal = await expectReady(page, "peer-management");

  await expect
    .poll(async () => Number(await terminal.getAttribute("data-simulated-max-remote-peer-download-bps")))
    .toBeGreaterThanOrEqual(5_000_000);
  expect(
    Number(await terminal.getAttribute("data-simulated-max-remote-peer-download-bps")),
  ).toBeLessThanOrEqual(330_000_000);
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-total-upload-bps")))
    .toBeGreaterThanOrEqual(40_000_000);
  expect(Number(await terminal.getAttribute("data-total-upload-bps"))).toBeLessThanOrEqual(
    330_000_000,
  );

  await expect
    .poll(async () => Number(await terminal.getAttribute("data-simulated-zero-progress-peers")), {
      timeout: 10_000,
    })
    .toBeGreaterThan(0);
  await expect
    .poll(async () => Number(await terminal.getAttribute("data-simulated-peer-download-starts")))
    .toBeGreaterThan(0);
});

test("one-and-a-half-gibibyte downloads share a variable three-hundred-megabit link", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/?scenario=downloading");
  const terminal = await expectReady(page);

  const totalSize = Number(await terminal.getAttribute("data-simulated-total-size"));
  expect(totalSize).toBe((3 * 1024 * 1024 * 1024) / 2);
  const rates = new Set<number>();
  let maximumRate = 0;
  for (let sample = 0; sample < 70; sample += 1) {
    const rate = Number(await terminal.getAttribute("data-total-download-bps"));
    expect(rate).toBeLessThanOrEqual(330_000_000);
    if (rate > 0) rates.add(rate);
    maximumRate = Math.max(maximumRate, rate);
    await page.waitForTimeout(100);
  }

  expect(maximumRate).toBeGreaterThanOrEqual(240_000_000);
  expect(rates.size).toBeGreaterThan(10);
  expect(errors).toEqual([]);
});

test("active DHT telemetry and weighted disk states reach the production visualizations", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await expect(terminal).toHaveAttribute("data-torrent-count", "15");

  const queryCounts = new Set<number>();
  const diskLevels = new Set<number>();
  const discoveryDeltas = new Set<number>();
  let activeDiscoverySamples = 0;
  let previousDiscovered = Number(await terminal.getAttribute("data-peer-discovered-events"));
  for (let sample = 0; sample < 100; sample += 1) {
    const queries = Number(await terminal.getAttribute("data-dht-active-queries"));
    queryCounts.add(queries);
    diskLevels.add(Number(await terminal.getAttribute("data-disk-health-state-level")));
    expect(queries).toBeGreaterThanOrEqual(72);
    expect(Number(await terminal.getAttribute("data-dht-peers-found"))).toBeGreaterThanOrEqual(2_000);
    const discovered = Number(await terminal.getAttribute("data-peer-discovered-events"));
    if (discovered > previousDiscovered) {
      discoveryDeltas.add(discovered - previousDiscovered);
      activeDiscoverySamples += 1;
    }
    previousDiscovered = discovered;
    await page.waitForTimeout(100);
  }

  expect(Number(await terminal.getAttribute("data-dht-query-load"))).toBeGreaterThan(0.5);
  expect(queryCounts.size).toBeGreaterThan(5);
  expect(previousDiscovered).toBeGreaterThan(5);
  expect(previousDiscovered).toBeLessThan(100);
  expect(activeDiscoverySamples).toBeGreaterThanOrEqual(4);
  expect(discoveryDeltas.size).toBeGreaterThanOrEqual(2);
  expect(Math.max(...discoveryDeltas)).toBeGreaterThan(Math.min(...discoveryDeltas));
  expect(Math.max(...discoveryDeltas)).toBeLessThanOrEqual(6);
  expect(diskLevels.has(1)).toBe(true);
  expect(diskLevels.has(2)).toBe(true);
  expect(diskLevels.has(3)).toBe(true);
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

  await expect(terminal).toHaveAttribute("data-torrent-count", "15");
  await page.evaluate(() => {
    const data = new DataTransfer();
    data.setData("text", "magnet:?xt=urn:btih:c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1");
    document.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }));
  });
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");

  await openScreen(page, "d", "delete-confirm");
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "15");
  expect(errors).toEqual([]);
});

test("dynamic torrent crosses the complete simulated lifecycle with coherent metrics", async ({ page }) => {
  test.setTimeout(90_000);
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
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");

  const phases = new Set<string>();
  const stalls = new Set<string>();
  let previousBytes = 0;
  let previousDownloadBps: number | undefined;
  let largestDownloadRateStep = 0;
  let sawActiveDownload = false;
  let sawDownloadAverageDuringPeerLull = false;
  const checkingDownloadRates: number[] = [];
  const checkingUploadRates: number[] = [];
  // Fifteen defaults and an interactive addition share the same 300 Mbps link. Preserve that
  // contention and allow the browser enough display-cadence samples to observe the full lifecycle.
  for (let sample = 0; sample < 720; sample += 1) {
    const state = await terminal.evaluate((element) => ({
      phase: element.dataset.simulatedPhase ?? "",
      stall: element.dataset.simulatedStall ?? "",
      bytes: Number(element.dataset.simulatedBytesWritten),
      total: Number(element.dataset.simulatedTotalSize),
      downloadBps: Number(element.dataset.simulatedDownloadBps),
      uploadBps: Number(element.dataset.simulatedUploadBps),
      peers: Number(element.dataset.simulatedPeers),
    }));
    const { phase, stall, bytes, total, downloadBps, uploadBps, peers } = state;
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
      checkingUploadRates.push(uploadBps);
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
  // Browser samples may span the 250 ms detail-publish boundary. A five-second EMA can therefore
  // move by just under 5% of a step input while still preserving the native smoothing contract.
  expect(largestDownloadRateStep).toBeLessThan(330_000_000 * 0.05);
  await expect(terminal).toHaveAttribute("data-simulated-complete", "true");
  expect(Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBe(
    Number(await terminal.getAttribute("data-simulated-total-size")),
  );
  expect(Number(await terminal.getAttribute("data-simulated-upload-bps"))).toBeGreaterThan(0);
  expect(Number(await terminal.getAttribute("data-visualization-phase"))).toBeGreaterThan(visualizationStart);
  expect(Number(await terminal.getAttribute("data-simulation-tick-count"))).toBeGreaterThan(simulationTicksStart);
  expect(Number(await terminal.getAttribute("data-network-history-samples"))).toBeGreaterThanOrEqual(120);
  expect(Number(await terminal.getAttribute("data-activity-history-samples"))).toBeGreaterThanOrEqual(120);
  expect(Number(await terminal.getAttribute("data-peer-connected-events"))).toBeGreaterThan(5);
  const lifecycleDiscoveries = Number(await terminal.getAttribute("data-peer-discovered-events"));
  expect(lifecycleDiscoveries).toBeGreaterThan(5);
  expect(lifecycleDiscoveries).toBeLessThan(100);
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
  await expect(terminal).toHaveAttribute("data-torrent-count", "16");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading", { timeout: 5_000 });
  await expect.poll(async () => Number(await terminal.getAttribute("data-simulated-bytes-written"))).toBeGreaterThan(0);

  const simulatedHash = await terminal.getAttribute("data-simulated-torrent-hash");
  expect(simulatedHash).not.toBe("");
  await page.keyboard.press("s");
  await expect(terminal).toHaveAttribute("data-torrent-sort-column", "name");
  await expect(terminal).toHaveAttribute("data-torrent-sort-pinned", "true");
  await page.keyboard.press("Home");
  for (let index = 0; index < 16; index += 1) {
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
  await expect(terminal).toHaveAttribute("data-torrent-count", "15");
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
      new Promise<{ progress: number; rates: number }>((resolve) => {
        const progressValues = new Set<string>();
        const rateValues = new Set<string>();
        const startedAt = performance.now();
        const sample = (now: number): void => {
          progressValues.add(element.dataset.simulatedBytesWritten ?? "");
          rateValues.add(element.dataset.simulatedDownloadBps ?? "");
          if (now - startedAt >= 400) {
            resolve({
              progress: progressValues.size,
              rates: rateValues.size,
            });
          } else requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
      }),
  );

  expect(frameSamples.progress).toBeGreaterThanOrEqual(12);
  expect(frameSamples.rates).toBeGreaterThanOrEqual(12);
  expect(errors).toEqual([]);
});

test("selected peer rates publish at frame cadence without peer-manager rebuild churn", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/?scenario=downloading");
  const terminal = await expectReady(page);
  await expect(terminal).toHaveAttribute("data-scenario-name", "downloading");
  await expect(terminal).toHaveAttribute("data-simulated-phase", "downloading");
  const changesBefore = Number(await terminal.getAttribute("data-peer-rate-frame-changes"));
  const managerUpdatesBefore = Number(await terminal.getAttribute("data-peer-manager-metrics-updates"));

  await page.waitForTimeout(400);

  const peerRateChanges =
    Number(await terminal.getAttribute("data-peer-rate-frame-changes")) - changesBefore;
  const peerManagerUpdates =
    Number(await terminal.getAttribute("data-peer-manager-metrics-updates")) - managerUpdatesBefore;
  expect(peerRateChanges).toBeGreaterThanOrEqual(12);
  expect(peerManagerUpdates).toBeLessThanOrEqual(2);
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
  expect(Number(await terminal.getAttribute("data-cols"))).toBeLessThan(120);
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

test("device-pixel-ratio zoom never exposes a cleared terminal canvas", async ({ page, context }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  const devtools = await context.newCDPSession(page);
  const paintSamples = terminal.evaluate(
    (element) =>
      new Promise<{ baseline: number; samples: number[] }>((resolve) => {
        const canvas = element.querySelector("canvas");
        if (!(canvas instanceof HTMLCanvasElement)) {
          resolve({ baseline: 0, samples: [] });
          return;
        }

        const sampleCanvas = document.createElement("canvas");
        sampleCanvas.width = 80;
        sampleCanvas.height = 50;
        const sampleContext = sampleCanvas.getContext("2d", { willReadFrequently: true });
        if (sampleContext === null) {
          resolve({ baseline: 0, samples: [] });
          return;
        }

        const paintedRatio = (): number => {
          sampleContext.clearRect(0, 0, sampleCanvas.width, sampleCanvas.height);
          sampleContext.drawImage(canvas, 0, 0, sampleCanvas.width, sampleCanvas.height);
          const pixels = sampleContext.getImageData(
            0,
            0,
            sampleCanvas.width,
            sampleCanvas.height,
          ).data;
          let painted = 0;
          for (let offset = 0; offset < pixels.length; offset += 4) {
            const colorDistance =
              Math.abs(pixels[offset] - 5) +
              Math.abs(pixels[offset + 1] - 7) +
              Math.abs(pixels[offset + 2] - 8);
            if (pixels[offset + 3] > 0 && colorDistance > 18) painted += 1;
          }
          return painted / (pixels.length / 4);
        };

        const baseline = paintedRatio();
        const samples: number[] = [];
        const observer = new MutationObserver(() => {
          queueMicrotask(() => samples.push(paintedRatio()));
        });
        observer.observe(canvas, { attributes: true, attributeFilter: ["width", "height"] });
        window.setTimeout(() => {
          observer.disconnect();
          resolve({ baseline, samples });
        }, 600);
      }),
  );

  await page.waitForTimeout(50);
  await devtools.send("Emulation.setDeviceMetricsOverride", {
    width: 1280,
    height: 800,
    deviceScaleFactor: 2,
    mobile: false,
  });

  const { baseline, samples } = await paintSamples;
  expect(baseline).toBeGreaterThan(0.01);
  expect(samples.length).toBeGreaterThan(0);
  expect(
    Math.min(...samples),
    `canvas paint ratios after DPR resize: ${JSON.stringify(samples)}`,
  ).toBeGreaterThanOrEqual(baseline * 0.25);
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
  expect(animationEnd - animationStart).toBeGreaterThanOrEqual(60);
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
