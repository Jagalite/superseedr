import { FitAddon, Terminal, init as initGhostty } from "ghostty-web";
import initSuperseedr, { BrowserDemo } from "../pkg/superseedr_web";
import "./style.css";

const FRAME_INTERVAL_MS = 1000 / 60;
const BACKGROUND_JUMP_MS = 250;
const PASTE_BURST_FLUSH_MS = 20;
const SETTLED_FIT_MS = 120;
const GEOMETRY_POLL_MS = 200;

const terminalHost = requireElement<HTMLDivElement>("terminal");
const status = requireElement<HTMLParagraphElement>("status");

interface MutableDevicePixelRatioRenderer {
  devicePixelRatio: number;
  resize(cols: number, rows: number): void;
}

class SerializedTerminalWriter {
  private writing = false;
  private activeWrites = 0;
  private peakConcurrentWrites = 0;

  constructor(
    private readonly terminal: Terminal,
    private readonly onStateChange: (busy: boolean) => void,
    private readonly completionDelayMs = 0,
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
      const finish = (): void => {
        this.activeWrites -= 1;
        this.writing = false;
        this.onStateChange(false);
      };
      if (this.completionDelayMs > 0) window.setTimeout(finish, this.completionDelayMs);
      else finish();
    });
    return true;
  }
}

async function start(): Promise<void> {
  await Promise.all([initGhostty(), initSuperseedr()]);
  const query = new URLSearchParams(window.location.search);
  const fontReadyDelayMs = boundedQueryNumber(query, "fontReadyDelayMs", 1_000);
  const layoutSettleDelayMs = boundedQueryNumber(query, "layoutSettleDelayMs", 1_000);

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
  status.textContent = "Waiting for terminal fonts…";
  await document.fonts.ready;
  if (fontReadyDelayMs > 0) await wait(fontReadyDelayMs);
  terminalHost.dataset.fontsReady = "true";
  fit.fit();
  fitCount += 1;
  terminalHost.dataset.fitCount = String(fitCount);
  terminalHost.dataset.lastFitSource = "initial-fonts";
  status.textContent = "Waiting for terminal layout…";
  await waitForSettledLayout(layoutSettleDelayMs);
  fit.fit();
  fitCount += 1;
  terminalHost.dataset.fitCount = String(fitCount);
  terminalHost.dataset.lastFitSource = "initial-settled";
  terminalHost.dataset.initialFitSettled = "true";

  const demo = new BrowserDemo(Math.max(1, terminal.cols), Math.max(1, terminal.rows));
  const requestedScenario = query.get("scenario");
  if (requestedScenario !== null && !demo.loadScenario(requestedScenario)) {
    throw new Error(`Unknown browser scenario: ${requestedScenario}`);
  }
  const requestedScreen = query.get("screen");
  if (requestedScreen !== null && !demo.showScreen(requestedScreen)) {
    throw new Error(`Unknown production screen: ${requestedScreen}`);
  }
  const writerDelayMs = Math.min(1_000, Math.max(0, Number(query.get("writerDelayMs")) || 0));
  const writer = new SerializedTerminalWriter(
    terminal,
    (busy) => {
      terminalHost.dataset.writeBusy = String(busy);
    },
    writerDelayMs,
  );
  let operationTail: Promise<void> = Promise.resolve();
  let pendingOperations = 0;
  let needsFullRefresh = true;
  let running = true;
  let animationFrameId = 0;
  let lastSimulationAt = 0;
  let frameCount = 0;
  let simulationTickCount = 0;
  let renderRequested = true;
  let flushTimer: number | undefined;
  let settledFitTimer: number | undefined;
  let lastDevicePixelRatio = window.devicePixelRatio;
  let resizeObserverCount = 0;
  let observedTerminalWidth = terminalHost.clientWidth;
  let observedTerminalHeight = terminalHost.clientHeight;
  let observedDevicePixelRatio = window.devicePixelRatio;
  let requestedColumns = demo.columns;
  let requestedRows = demo.rows;

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
    terminalHost.dataset.simulationTickCount = String(simulationTickCount);
    terminalHost.dataset.writeBusy = String(writer.busy);
    terminalHost.dataset.maxConcurrentWrites = String(writer.maxConcurrentWrites);
    terminalHost.dataset.fitCount = String(fitCount);
    terminalHost.dataset.resizeObserverCount = String(resizeObserverCount);
    terminalHost.dataset.devicePixelRatio = String(window.devicePixelRatio);
    terminalHost.dataset.rendererDevicePixelRatio = String(rendererDevicePixelRatio(terminal));
    terminalHost.dataset.currentTheme = demo.currentTheme;
    terminalHost.dataset.targetFps = String(demo.targetFps);
    terminalHost.dataset.fpsLabel = demo.fpsLabel;
    terminalHost.dataset.scenarioName = demo.scenarioName;
    terminalHost.dataset.scenarioMetadataCount = String(demo.scenarioMetadataCount);
    terminalHost.dataset.scenarioPeerDiscoveryCount = String(demo.scenarioPeerDiscoveryCount);
    terminalHost.dataset.scenarioDownloadingCount = String(demo.scenarioDownloadingCount);
    terminalHost.dataset.scenarioCheckingCount = String(demo.scenarioCheckingCount);
    terminalHost.dataset.scenarioSeedingCount = String(demo.scenarioSeedingCount);
    terminalHost.dataset.scenarioPausedCount = String(demo.scenarioPausedCount);
    terminalHost.dataset.scenarioDeletingCount = String(demo.scenarioDeletingCount);
    terminalHost.dataset.scenarioMaxPeers = String(demo.scenarioMaxPeers);
    terminalHost.dataset.scenarioPeerRateVariants = String(demo.scenarioPeerRateVariants);
    terminalHost.dataset.scenarioAvailabilityLevels = String(demo.scenarioAvailabilityLevels);
    terminalHost.dataset.scenarioPieceAcquisitions = String(demo.scenarioPieceAcquisitions);
    terminalHost.dataset.scenarioMissingPieces = String(demo.scenarioMissingPieces);
    terminalHost.dataset.scenarioDiskState = demo.scenarioDiskState;
    terminalHost.dataset.scenarioWarning = String(demo.scenarioWarning);
    terminalHost.dataset.scenarioRecovered = String(demo.scenarioRecovered);
    terminalHost.dataset.selectedTorrentPaused = String(demo.selectedTorrentPaused);
    terminalHost.dataset.selectedTorrentHash = demo.selectedTorrentHash;
    terminalHost.dataset.simulatedTorrentHash = demo.simulatedTorrentHash;
    terminalHost.dataset.simulatedTorrentPaused = String(demo.simulatedTorrentPaused);
    terminalHost.dataset.torrentCount = String(demo.torrentCount);
    terminalHost.dataset.torrentSortColumn = demo.torrentSortColumn;
    terminalHost.dataset.torrentSortPinned = String(demo.torrentSortPinned);
    terminalHost.dataset.torrentSortDirection = demo.torrentSortDirection;
    terminalHost.dataset.orderedTorrentDownloadRates = demo.orderedTorrentDownloadRates;
    terminalHost.dataset.orderedTorrentUploadRates = demo.orderedTorrentUploadRates;
    terminalHost.dataset.defaultDownloadFolder = demo.defaultDownloadFolder;
    terminalHost.dataset.currentScreen = demo.currentScreen;
    terminalHost.dataset.simulatedPhase = demo.simulatedPhase;
    terminalHost.dataset.simulatedStall = demo.simulatedStall;
    terminalHost.dataset.simulatedActivity = demo.simulatedActivity;
    terminalHost.dataset.simulatedBytesWritten = String(demo.simulatedBytesWritten);
    terminalHost.dataset.simulatedTotalSize = String(demo.simulatedTotalSize);
    terminalHost.dataset.simulatedDownloadBps = String(demo.simulatedDownloadBps);
    terminalHost.dataset.simulatedUploadBps = String(demo.simulatedUploadBps);
    terminalHost.dataset.simulatedBytesDownloadedTick = String(demo.simulatedBytesDownloadedTick);
    terminalHost.dataset.simulatedEtaSeconds = String(demo.simulatedEtaSeconds);
    terminalHost.dataset.simulatedAnnounceSeconds = String(demo.simulatedAnnounceSeconds);
    terminalHost.dataset.simulatedPeers = String(demo.simulatedPeers);
    terminalHost.dataset.simulatedTcpPeers = String(demo.simulatedTcpPeers);
    terminalHost.dataset.simulatedUtpPeers = String(demo.simulatedUtpPeers);
    terminalHost.dataset.simulatedBeneficialPeers = String(demo.simulatedBeneficialPeers);
    terminalHost.dataset.simulatedUploadRecipients = String(demo.simulatedUploadRecipients);
    terminalHost.dataset.simulatedComplete = String(demo.simulatedComplete);
    terminalHost.dataset.torrentPreviewState = demo.torrentPreviewState;
    terminalHost.dataset.torrentPreviewName = demo.torrentPreviewName;
    terminalHost.dataset.torrentPreviewFileCount = String(demo.torrentPreviewFileCount);
    terminalHost.dataset.visualizationPhase = String(demo.visualizationPhase);
    terminalHost.dataset.networkHistorySamples = String(demo.networkHistorySamples);
    terminalHost.dataset.activityHistorySamples = String(demo.activityHistorySamples);
    terminalHost.dataset.peerConnectedEvents = String(demo.peerConnectedEvents);
    terminalHost.dataset.peerDiscoveredEvents = String(demo.peerDiscoveredEvents);
    terminalHost.dataset.peerDisconnectedEvents = String(demo.peerDisconnectedEvents);
    terminalHost.dataset.recentFileActivity = String(demo.recentFileActivity);
    terminalHost.dataset.recentFileDownloadActivity = String(demo.recentFileDownloadActivity);
    terminalHost.dataset.recentFileUploadActivity = String(demo.recentFileUploadActivity);
    terminalHost.dataset.blocksReceivedEvents = String(demo.blocksReceivedEvents);
    terminalHost.dataset.blocksSentEvents = String(demo.blocksSentEvents);
    terminalHost.dataset.readIops = String(demo.readIops);
    terminalHost.dataset.writeIops = String(demo.writeIops);
    terminalHost.dataset.diskReadLatencyMicros = String(demo.diskReadLatencyMicros);
    terminalHost.dataset.diskWriteLatencyMicros = String(demo.diskWriteLatencyMicros);
    terminalHost.dataset.recvToWriteLatencyMicros = String(demo.recvToWriteLatencyMicros);
    terminalHost.dataset.trackedPeers = String(demo.trackedPeers);
    terminalHost.dataset.swarmAvailabilitySamples = String(demo.swarmAvailabilitySamples);
    terminalHost.dataset.dhtWaveInitialized = String(demo.dhtWaveInitialized);
  };

  const render = (now: number): void => {
    animationFrameId = 0;
    if (!running) return;

    const elapsed = now - lastSimulationAt;
    if (elapsed > BACKGROUND_JUMP_MS) {
      lastSimulationAt = now - FRAME_INTERVAL_MS;
      needsFullRefresh = true;
    }

    if (
      document.visibilityState === "visible" &&
      elapsed > 0 &&
      pendingOperations === 0
    ) {
      const simulationDelta =
        elapsed > BACKGROUND_JUMP_MS
          ? FRAME_INTERVAL_MS / 1000
          : Math.min(elapsed / 1000, 0.1);
      demo.advanceSimulation(simulationDelta);
      simulationTickCount += 1;
      renderRequested = true;
      lastSimulationAt = now;
      updateDiagnostics();
    }
    if (
      document.visibilityState === "visible" &&
      renderRequested &&
      pendingOperations === 0 &&
      !writer.busy
    ) {
      const frame = needsFullRefresh ? demo.forceRefresh() : demo.renderFrame();
      needsFullRefresh = false;
      if (writer.write(frame)) frameCount += 1;
      renderRequested = false;
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
    if (nextCols === requestedColumns && nextRows === requestedRows) return;
    requestedColumns = nextCols;
    requestedRows = nextRows;
    enqueue(async () => {
      await demo.resize(nextCols, nextRows);
      needsFullRefresh = true;
      renderRequested = true;
      updateDiagnostics();
    });
  };

  const fitTerminal = (source: string): void => {
    if (!running) return;
    const pixelRatioChanged = synchronizeRendererDevicePixelRatio(terminal);
    fit.fit();
    fitCount += 1;
    terminalHost.dataset.lastFitSource = source;
    if (pixelRatioChanged) {
      needsFullRefresh = true;
      renderRequested = true;
    }
    forwardResize(terminal.cols, terminal.rows);
    updateDiagnostics();
  };

  const scheduleFit = (source = "layout"): void => {
    fitTerminal(`${source}:immediate`);
    window.clearTimeout(settledFitTimer);
    settledFitTimer = window.setTimeout(() => {
      settledFitTimer = undefined;
      fitTerminal(`${source}:settled`);
    }, SETTLED_FIT_MS);
  };

  const handleWindowResize = (): void => scheduleFit("window-resize");
  const handleVisualViewportResize = (): void => scheduleFit("visual-viewport");
  const terminalResizeObserver =
    typeof ResizeObserver === "undefined"
      ? undefined
      : new ResizeObserver(() => {
          resizeObserverCount += 1;
          scheduleFit("resize-observer");
        });
  terminalResizeObserver?.observe(terminalHost);

  terminal.onResize(({ cols, rows }) => forwardResize(cols, rows));
  window.addEventListener("resize", handleWindowResize, { passive: true });
  window.visualViewport?.addEventListener("resize", handleVisualViewportResize, { passive: true });

  let devicePixelRatioQuery: MediaQueryList | undefined;
  const watchDevicePixelRatio = (): void => {
    devicePixelRatioQuery = window.matchMedia(`(resolution: ${lastDevicePixelRatio}dppx)`);
    devicePixelRatioQuery.addEventListener("change", handleDevicePixelRatioChange, { once: true });
  };
  const handleDevicePixelRatioChange = (): void => {
    lastDevicePixelRatio = window.devicePixelRatio;
    scheduleFit("device-pixel-ratio");
    watchDevicePixelRatio();
  };
  watchDevicePixelRatio();

  const terminalGeometryTimer = window.setInterval(() => {
    const width = terminalHost.clientWidth;
    const height = terminalHost.clientHeight;
    const devicePixelRatio = window.devicePixelRatio;
    if (
      width === observedTerminalWidth &&
      height === observedTerminalHeight &&
      devicePixelRatio === observedDevicePixelRatio
    ) {
      return;
    }

    observedTerminalWidth = width;
    observedTerminalHeight = height;
    observedDevicePixelRatio = devicePixelRatio;
    if (devicePixelRatio !== lastDevicePixelRatio) lastDevicePixelRatio = devicePixelRatio;
    scheduleFit("geometry-fallback");
  }, GEOMETRY_POLL_MS);

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
  const preventTerminalScroll = (event: Event): void => {
    event.preventDefault();
    terminalHost.dataset.scrollBlockedCount = String(
      Number(terminalHost.dataset.scrollBlockedCount ?? 0) + 1,
    );
  };
  terminalHost.addEventListener("wheel", preventTerminalScroll, { capture: true, passive: false });
  terminalHost.addEventListener("touchmove", preventTerminalScroll, {
    capture: true,
    passive: false,
  });

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") {
      lastSimulationAt = performance.now();
      needsFullRefresh = true;
      renderRequested = true;
      scheduleFit();
    }
  });
  window.addEventListener("pageshow", () => {
    running = true;
    lastSimulationAt = performance.now();
    needsFullRefresh = true;
    renderRequested = true;
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
    window.clearTimeout(settledFitTimer);
    window.clearInterval(terminalGeometryTimer);
    terminalResizeObserver?.disconnect();
    window.removeEventListener("resize", handleWindowResize);
    window.visualViewport?.removeEventListener("resize", handleVisualViewportResize);
    devicePixelRatioQuery?.removeEventListener("change", handleDevicePixelRatioChange);
    terminalHost.removeEventListener("wheel", preventTerminalScroll, { capture: true });
    terminalHost.removeEventListener("touchmove", preventTerminalScroll, { capture: true });
    demo.free();
    fit.dispose();
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

function rendererDevicePixelRatio(terminal: Terminal): number {
  return mutableDevicePixelRatioRenderer(terminal)?.devicePixelRatio ?? window.devicePixelRatio;
}

function synchronizeRendererDevicePixelRatio(terminal: Terminal): boolean {
  const renderer = mutableDevicePixelRatioRenderer(terminal);
  const nextDevicePixelRatio = window.devicePixelRatio;
  if (
    renderer === undefined ||
    !Number.isFinite(nextDevicePixelRatio) ||
    nextDevicePixelRatio <= 0 ||
    Math.abs(renderer.devicePixelRatio - nextDevicePixelRatio) < 0.001
  ) {
    return false;
  }

  // ghostty-web 0.4 captures DPR when its renderer is constructed but has no public DPR setter.
  // Keep its backing canvas synchronized when browser zoom changes DPR without recreating the
  // terminal, which would discard the retained buffer and input lifecycle.
  renderer.devicePixelRatio = nextDevicePixelRatio;
  renderer.resize(terminal.cols, terminal.rows);
  return true;
}

function mutableDevicePixelRatioRenderer(
  terminal: Terminal,
): MutableDevicePixelRatioRenderer | undefined {
  const renderer = terminal.renderer as unknown as Partial<MutableDevicePixelRatioRenderer> | undefined;
  return renderer !== undefined &&
    typeof renderer.devicePixelRatio === "number" &&
    typeof renderer.resize === "function"
    ? (renderer as MutableDevicePixelRatioRenderer)
    : undefined;
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

function boundedQueryNumber(query: URLSearchParams, name: string, maximum: number): number {
  return Math.min(maximum, Math.max(0, Number(query.get(name)) || 0));
}

function wait(milliseconds: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
}

async function waitForSettledLayout(additionalDelayMs: number): Promise<void> {
  await new Promise<void>((resolve) => {
    let settled = false;
    const finish = (): void => {
      if (settled) return;
      settled = true;
      resolve();
    };
    window.requestAnimationFrame(finish);
    window.setTimeout(finish, 50);
  });
  if (additionalDelayMs > 0) {
    await wait(additionalDelayMs);
    await new Promise<void>((resolve) => window.requestAnimationFrame(() => resolve()));
  }
}

void start().catch((error: unknown) => {
  console.error(error);
  status.textContent = "Unable to initialize the demo.";
  status.dataset.ready = "false";
});
