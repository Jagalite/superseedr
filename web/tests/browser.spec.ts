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

test("paste pause resume and confirmed deletion preserve browser reducer state", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/");
  const terminal = await expectReady(page);
  await terminal.click();

  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "true");
  await page.keyboard.press("p");
  await expect(terminal).toHaveAttribute("data-selected-torrent-paused", "false");

  await expect(terminal).toHaveAttribute("data-torrent-count", "6");
  await page.evaluate(() => {
    const data = new DataTransfer();
    data.setData("text", "magnet:?xt=urn:btih:c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1");
    document.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }));
  });
  await expect(terminal).toHaveAttribute("data-torrent-count", "7");

  await openScreen(page, "d", "delete-confirm");
  await expect(terminal).toHaveAttribute("data-torrent-count", "7");
  await page.keyboard.press("Shift+Y");
  await expect(terminal).toHaveAttribute("data-current-screen", "normal");
  await expect(terminal).toHaveAttribute("data-torrent-count", "6");
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
  expect(animationEnd - animationStart).toBeGreaterThan(10);
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
