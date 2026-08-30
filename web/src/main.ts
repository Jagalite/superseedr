import { FitAddon, Terminal, init as initGhostty } from "ghostty-web";
import initSuperseedr, { BrowserDemo } from "../pkg/superseedr_web";
import "./style.css";

const FRAME_INTERVAL_MS = 1000 / 60;
const BACKGROUND_JUMP_MS = 250;
const PASTE_BURST_FLUSH_MS = 20;

const terminalHost = requireElement<HTMLDivElement>("terminal");
const status = requireElement<HTMLParagraphElement>("status");

class SerializedTerminalWriter {
  private writing = false;
  private activeWrites = 0;
  private peakConcurrentWrites = 0;

  constructor(
    private readonly terminal: Terminal,
    private readonly onStateChange: (busy: boolean) => void,
  ) {}

  get busy(): boolean {
    return this.writing;
  }

  get maxConcurrentWrites(): number {
    return this.peakConcurrentWrites;
  }

  write(frame: string): boolean {
    if (this.writing || frame.length === 0) return false;
    this.writing = true;
    this.activeWrites += 1;
    this.peakConcurrentWrites = Math.max(this.peakConcurrentWrites, this.activeWrites);
    this.onStateChange(true);
    this.terminal.write(frame, () => {
      this.activeWrites -= 1;
      this.writing = false;
      this.onStateChange(false);
    });
    return true;
  }
}

async function start(): Promise<void> {
  await Promise.all([initGhostty(), initSuperseedr()]);

  const terminal = new Terminal({
    cols: 120,
    rows: 40,
    cursorBlink: false,
    scrollback: 0,
    fontSize: 13,
    fontFamily: '"SFMono-Regular", "Cascadia Mono", "Liberation Mono", monospace',
    theme: {
      background: "#050708",
      foreground: "#d9e3de",
      cursor: "#68d8a7",
      black: "#101416",
      brightBlack: "#65716c",
      green: "#68d8a7",
      brightGreen: "#93e7c1",
      cyan: "#5cc9cf",
      brightCyan: "#8ae5e8",
      yellow: "#d7bd6a",
      brightYellow: "#ecd786",
    },
  });
  const fit = new FitAddon();
  terminal.loadAddon(fit);
  terminal.open(terminalHost);
  let fitCount = 0;
  fit.fit();
  fitCount += 1;

  const demo = new BrowserDemo(Math.max(1, terminal.cols), Math.max(1, terminal.rows));
  const requestedScreen = new URLSearchParams(window.location.search).get("screen");
  if (requestedScreen !== null && !demo.showScreen(requestedScreen)) {
    throw new Error(`Unknown production screen: ${requestedScreen}`);
  }
  const writer = new SerializedTerminalWriter(terminal, (busy) => {
    terminalHost.dataset.writeBusy = String(busy);
  });
  let operationTail: Promise<void> = Promise.resolve();
  let pendingOperations = 0;
  let needsFullRefresh = true;
  let running = true;
  let animationFrameId = 0;
  let lastFrameAt = 0;
  let frameCount = 0;
  let flushTimer: number | undefined;
  let resizeTimer: number | undefined;
  let lastDevicePixelRatio = window.devicePixelRatio;

  const reportError = (error: unknown): void => {
    console.error(error);
    status.textContent = "Demo paused after an unexpected error.";
    status.dataset.ready = "false";
  };

  const enqueue = (operation: () => void | Promise<void>): void => {
    pendingOperations += 1;
    operationTail = operationTail
      .then(operation)
      .catch(reportError)
      .finally(() => {
        pendingOperations -= 1;
      });
  };

  const updateDiagnostics = (): void => {
    terminalHost.dataset.cols = String(demo.columns);
    terminalHost.dataset.rows = String(demo.rows);
    terminalHost.dataset.frameCount = String(frameCount);
    terminalHost.dataset.writeBusy = String(writer.busy);
    terminalHost.dataset.maxConcurrentWrites = String(writer.maxConcurrentWrites);
    terminalHost.dataset.fitCount = String(fitCount);
    terminalHost.dataset.devicePixelRatio = String(window.devicePixelRatio);
    terminalHost.dataset.selectedTorrentPaused = String(demo.selectedTorrentPaused);
    terminalHost.dataset.torrentCount = String(demo.torrentCount);
    terminalHost.dataset.defaultDownloadFolder = demo.defaultDownloadFolder;
    terminalHost.dataset.currentScreen = demo.currentScreen;
  };

  const render = (now: number): void => {
    animationFrameId = 0;
    if (!running) return;

    const elapsed = now - lastFrameAt;
    if (elapsed > BACKGROUND_JUMP_MS) {
      lastFrameAt = now - FRAME_INTERVAL_MS;
      needsFullRefresh = true;
    }

    if (
      document.visibilityState === "visible" &&
      elapsed >= FRAME_INTERVAL_MS &&
      pendingOperations === 0 &&
      !writer.busy
    ) {
      const frame = needsFullRefresh ? demo.forceRefresh() : demo.renderFrame();
      needsFullRefresh = false;
      if (writer.write(frame)) frameCount += 1;
      lastFrameAt = now;
      updateDiagnostics();
    }
    animationFrameId = requestAnimationFrame(render);
  };

  const startAnimation = (): void => {
    if (animationFrameId === 0) animationFrameId = requestAnimationFrame(render);
  };

  const stopAnimation = (): void => {
    if (animationFrameId !== 0) cancelAnimationFrame(animationFrameId);
    animationFrameId = 0;
  };

  const forwardResize = (cols: number, rows: number): void => {
    const nextCols = Math.max(1, cols);
    const nextRows = Math.max(1, rows);
    if (nextCols === demo.columns && nextRows === demo.rows) return;
    enqueue(async () => {
      await demo.resize(nextCols, nextRows);
      needsFullRefresh = true;
      updateDiagnostics();
    });
  };

  const scheduleFit = (): void => {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(() => {
      fit.fit();
      fitCount += 1;
      forwardResize(terminal.cols, terminal.rows);
      updateDiagnostics();
    }, 32);
  };

  terminal.onResize(({ cols, rows }) => forwardResize(cols, rows));
  window.addEventListener("resize", scheduleFit, { passive: true });
  window.visualViewport?.addEventListener("resize", scheduleFit, { passive: true });

  const watchDevicePixelRatio = (): void => {
    const query = window.matchMedia(`(resolution: ${lastDevicePixelRatio}dppx)`);
    query.addEventListener(
      "change",
      () => {
        lastDevicePixelRatio = window.devicePixelRatio;
        scheduleFit();
        watchDevicePixelRatio();
      },
      { once: true },
    );
  };
  watchDevicePixelRatio();

  const scheduleInputFlush = (): void => {
    window.clearTimeout(flushTimer);
    flushTimer = window.setTimeout(
      () =>
        enqueue(async () => {
          await demo.flushInput();
          terminalHost.dataset.inputFlushCount = String(Number(terminalHost.dataset.inputFlushCount ?? 0) + 1);
        }),
      PASTE_BURST_FLUSH_MS,
    );
  };

  document.addEventListener(
    "keydown",
    (event) => {
      if (!terminalHost.contains(document.activeElement)) return;
      if (event.isComposing || isBrowserShortcut(event, terminal) || isModifierOnly(event.key)) return;
      event.preventDefault();
      terminalHost.dataset.lastKey = event.key;
      const modifierBits = eventModifiers(event);
      enqueue(async () => {
        const handled = await demo.dispatchKey(event.key, modifierBits, event.repeat ? 1 : 0);
        terminalHost.dataset.lastKeyHandled = String(handled);
        if (handled) {
          scheduleInputFlush();
        }
      });
    },
    { capture: true },
  );

  document.addEventListener(
    "keyup",
    (event) => {
      if (!terminalHost.contains(document.activeElement)) return;
      if (event.isComposing || isBrowserShortcut(event, terminal) || isModifierOnly(event.key)) return;
      event.preventDefault();
      const modifierBits = eventModifiers(event);
      enqueue(async () => {
        await demo.dispatchKey(event.key, modifierBits, 2);
      });
    },
    { capture: true },
  );

  document.addEventListener(
    "paste",
    (event) => {
      if (!terminalHost.contains(document.activeElement)) return;
      const text = event.clipboardData?.getData("text") ?? "";
      if (text.length === 0) return;
      event.preventDefault();
      enqueue(() => demo.dispatchPaste(text));
    },
    { capture: true },
  );
  terminalHost.addEventListener("click", () => queueMicrotask(() => terminal.focus()));

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") {
      lastFrameAt = performance.now();
      needsFullRefresh = true;
      scheduleFit();
    }
  });
  window.addEventListener("pageshow", () => {
    running = true;
    lastFrameAt = performance.now();
    needsFullRefresh = true;
    scheduleFit();
    startAnimation();
  });
  window.addEventListener("pagehide", () => {
    running = false;
    stopAnimation();
  });
  window.addEventListener("beforeunload", () => {
    running = false;
    stopAnimation();
    window.clearTimeout(flushTimer);
    window.clearTimeout(resizeTimer);
    demo.free();
    terminal.dispose();
  });

  status.textContent = "Interactive demo ready";
  status.dataset.ready = "true";
  terminalHost.dataset.ready = "true";
  updateDiagnostics();
  terminal.focus();
  startAnimation();
}

function eventModifiers(event: KeyboardEvent): number {
  return (event.shiftKey ? 1 : 0) | (event.ctrlKey ? 2 : 0) | (event.altKey ? 4 : 0) | (event.metaKey ? 8 : 0);
}

function isModifierOnly(key: string): boolean {
  return key === "Shift" || key === "Control" || key === "Alt" || key === "Meta";
}

function isBrowserShortcut(event: KeyboardEvent, terminal: Terminal): boolean {
  if (event.metaKey) return true;
  return event.ctrlKey && event.key.toLowerCase() === "c" && terminal.hasSelection();
}

function requireElement<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!(element instanceof HTMLElement)) throw new Error(`missing #${id}`);
  return element as T;
}

void start().catch((error: unknown) => {
  console.error(error);
  status.textContent = "Unable to initialize the demo.";
  status.dataset.ready = "false";
});
