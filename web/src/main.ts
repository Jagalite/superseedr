import {
  CellFlags,
  FitAddon,
  Terminal,
  init as initGhostty,
  type GhosttyCell,
} from "ghostty-web";
import initSuperseedr, { BrowserDemo } from "../pkg/superseedr_web";
import "./style.css";

const FRAME_INTERVAL_MS = 1000 / 60;
const BACKGROUND_JUMP_MS = 250;
const PASTE_BURST_FLUSH_MS = 20;
const SETTLED_FIT_MS = 120;
const GEOMETRY_POLL_MS = 200;
const DIAGNOSTIC_INTERVAL_MS = 100;

const terminalHost = requireElement<HTMLDivElement>("terminal");
const status = requireElement<HTMLParagraphElement>("status");
const terminalFrame = requireSelector<HTMLElement>(".terminal-frame");

interface MutableDevicePixelRatioRenderer {
  devicePixelRatio: number;
  resize(cols: number, rows: number): void;
}

interface GhosttyRenderBuffer {
  getCursor(): { x: number; y: number };
  getDimensions(): { cols: number; rows: number };
  isRowDirty(row: number): boolean;
  needsFullRedraw?(): boolean;
}

interface GhosttySelectionState {
  hasSelection(): boolean;
  getDirtySelectionRows(): Set<number>;
}

interface FixedCellCanvasRenderer {
  render(
    buffer: GhosttyRenderBuffer,
    forceAll?: boolean,
    viewportY?: number,
    scrollbackProvider?: unknown,
    scrollbarOpacity?: number,
  ): void;
  renderLine(line: unknown, row: number, columns: number): void;
  renderCellText(cell: GhosttyCell, column: number, row: number): void;
  cursorBlink: boolean;
  lastCursorPosition: { x: number; y: number };
  selectionManager?: GhosttySelectionState;
  hoveredHyperlinkId: number;
  previousHoveredHyperlinkId: number;
  hoveredLinkRange: unknown | null;
  previousHoveredLinkRange: unknown | null;
  canvas: HTMLCanvasElement;
  metrics: { width: number; height: number };
  devicePixelRatio: number;
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
    const finish = (): void => {
      this.activeWrites -= 1;
      this.writing = false;
      this.onStateChange(false);
    };
    try {
      if (this.completionDelayMs > 0) {
        this.terminal.write(frame, () => window.setTimeout(finish, this.completionDelayMs));
      } else {
        // ghostty-web parses writes synchronously; its optional callback is deferred to the next
        // animation frame. Waiting for that callback would incorrectly suppress the next frame.
        this.terminal.write(frame);
        finish();
      }
    } catch (error) {
      finish();
      throw error;
    }
    return true;
  }
}

async function start(): Promise<void> {
  await Promise.all([initGhostty(), initSuperseedr()]);
  const query = new URLSearchParams(window.location.search);
  const usesTouchKeyboard = window.matchMedia("(pointer: coarse)").matches;
  const fontReadyDelayMs = boundedQueryNumber(query, "fontReadyDelayMs", 1_000);
  const layoutSettleDelayMs = boundedQueryNumber(query, "layoutSettleDelayMs", 1_000);

  const terminal = new Terminal({
    cols: 120,
    rows: 40,
    cursorBlink: false,
    scrollback: 0,
    fontSize: 10,
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
  terminalHost.setAttribute("contenteditable", usesTouchKeyboard ? "false" : "plaintext-only");
  terminalHost.setAttribute("spellcheck", "false");
  terminalHost.dataset.clipboardTarget = usesTouchKeyboard ? "disabled" : "terminal-host";
  terminal.write("\x1b[?25l");
  terminalHost.dataset.cursorHidden = "true";
  terminalHost.dataset.inputFocusPolicy = usesTouchKeyboard ? "tap" : "automatic";
  if (usesTouchKeyboard) window.setTimeout(() => terminal.blur(), 0);
  installFixedCellDirtyRowRenderer(terminal);
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
  let pendingInputOperations = 0;
  let needsFullRefresh = true;
  let running = true;
  let animationFrameId = 0;
  let lastSimulationAt = 0;
  let lastAnimationAt = 0;
  let frameCount = 0;
  let simulationTickCount = 0;
  let lastDiagnosticsAt = 0;
  let renderRequested = true;
  let flushTimer: number | undefined;
  let settledFitTimer: number | undefined;
  let immediateFitAnimationFrameId = 0;
  let pendingFitSource = "layout";
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

  const enqueueInput = (operation: () => void | Promise<void>): void => {
    pendingInputOperations += 1;
    enqueue(async () => {
      try {
        await operation();
      } finally {
        pendingInputOperations -= 1;
        renderRequested = true;
        updateDiagnostics();
      }
    });
  };

  const inputSequencePending = (): boolean =>
    pendingInputOperations > 0 || flushTimer !== undefined;

  const setDiagnostic = (name: string, value: string): void => {
    if (terminalHost.dataset[name] !== value) terminalHost.dataset[name] = value;
  };

  const updateFrameDiagnostics = (): void => {
    setDiagnostic("frameCount", String(frameCount));
    setDiagnostic("simulationTickCount", String(simulationTickCount));
    setDiagnostic("fpsLabel", demo.fpsLabel);
    setDiagnostic("simulatedBytesWritten", String(demo.simulatedBytesWritten));
    setDiagnostic("simulatedDownloadBps", String(demo.simulatedDownloadBps));
    setDiagnostic("visualizationPhase", String(demo.visualizationPhase));
    setDiagnostic("peerRateFrameUpdates", String(demo.selectedPeerRateFrameUpdates));
    setDiagnostic("peerRateFrameChanges", String(demo.selectedPeerRateFrameChanges));
    setDiagnostic("peerManagerMetricsUpdates", String(demo.peerManagerMetricsUpdates));
  };

  const updateDiagnostics = (): void => {
    updateFrameDiagnostics();
    setDiagnostic("cols", String(demo.columns));
    setDiagnostic("rows", String(demo.rows));
    terminalHost.dataset.writeBusy = String(writer.busy);
    terminalHost.dataset.maxConcurrentWrites = String(writer.maxConcurrentWrites);
    terminalHost.dataset.fitCount = String(fitCount);
    terminalHost.dataset.resizeObserverCount = String(resizeObserverCount);
    terminalHost.dataset.devicePixelRatio = String(window.devicePixelRatio);
    terminalHost.dataset.rendererDevicePixelRatio = String(rendererDevicePixelRatio(terminal));
    terminalHost.dataset.fontSize = String(terminal.options.fontSize);
    terminalHost.dataset.currentTheme = demo.currentTheme;
    terminalHost.dataset.effectiveDownloadLimitBps = String(demo.effectiveDownloadLimitBps);
    terminalHost.dataset.configuredUploadLimitBps = String(demo.configuredUploadLimitBps);
    terminalHost.dataset.targetFps = String(demo.targetFps);
    terminalHost.dataset.browserNetworkInterfaceCount = String(
      demo.browserNetworkInterfaceCount,
    );
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
    terminalHost.dataset.rssFeedCount = String(demo.rssFeedCount);
    terminalHost.dataset.rssEnabledFeedCount = String(demo.rssEnabledFeedCount);
    terminalHost.dataset.rssHistoryCount = String(demo.rssHistoryCount);
    terminalHost.dataset.rssDownloadedPreviewCount = String(demo.rssDownloadedPreviewCount);
    terminalHost.dataset.rssLastSyncAt = demo.rssLastSyncAt;
    terminalHost.dataset.systemError = demo.systemError;
    terminalHost.dataset.torrentSortColumn = demo.torrentSortColumn;
    terminalHost.dataset.torrentSortPinned = String(demo.torrentSortPinned);
    terminalHost.dataset.torrentSortDirection = demo.torrentSortDirection;
    terminalHost.dataset.orderedTorrentDownloadRates = demo.orderedTorrentDownloadRates;
    terminalHost.dataset.orderedTorrentUploadRates = demo.orderedTorrentUploadRates;
    terminalHost.dataset.defaultDownloadFolder = demo.defaultDownloadFolder;
    terminalHost.dataset.currentScreen = demo.currentScreen;
    terminalHost.dataset.webQuitKeyEnabled = String(demo.webQuitKeyEnabled);
    terminalHost.dataset.shouldQuit = String(demo.shouldQuit);
    terminalHost.dataset.simulatedPhase = demo.simulatedPhase;
    terminalHost.dataset.simulatedStall = demo.simulatedStall;
    terminalHost.dataset.simulatedActivity = demo.simulatedActivity;
    terminalHost.dataset.simulatedTorrentName = demo.simulatedTorrentName;
    terminalHost.dataset.simulatedTotalSize = String(demo.simulatedTotalSize);
    terminalHost.dataset.simulatedUploadBps = String(demo.simulatedUploadBps);
    terminalHost.dataset.simulatedBytesDownloadedTick = String(demo.simulatedBytesDownloadedTick);
    terminalHost.dataset.simulatedEtaSeconds = String(demo.simulatedEtaSeconds);
    terminalHost.dataset.simulatedAnnounceSeconds = String(demo.simulatedAnnounceSeconds);
    terminalHost.dataset.simulatedPeers = String(demo.simulatedPeers);
    terminalHost.dataset.simulatedTcpPeers = String(demo.simulatedTcpPeers);
    terminalHost.dataset.simulatedUtpPeers = String(demo.simulatedUtpPeers);
    terminalHost.dataset.simulatedBeneficialPeers = String(demo.simulatedBeneficialPeers);
    terminalHost.dataset.simulatedUploadRecipients = String(demo.simulatedUploadRecipients);
    terminalHost.dataset.simulatedMaxRemotePeerDownloadBps = String(
      demo.simulatedMaxRemotePeerDownloadBps,
    );
    terminalHost.dataset.simulatedZeroProgressPeers = String(demo.simulatedZeroProgressPeers);
    terminalHost.dataset.simulatedPeerDownloadStarts = String(demo.simulatedPeerDownloadStarts);
    terminalHost.dataset.simulatedComplete = String(demo.simulatedComplete);
    terminalHost.dataset.torrentPreviewState = demo.torrentPreviewState;
    terminalHost.dataset.torrentPreviewName = demo.torrentPreviewName;
    terminalHost.dataset.torrentPreviewFileCount = String(demo.torrentPreviewFileCount);
    terminalHost.dataset.totalDownloadBps = String(demo.totalDownloadBps);
    terminalHost.dataset.totalUploadBps = String(demo.totalUploadBps);
    terminalHost.dataset.aggregateSessionDownloaded = String(demo.aggregateSessionDownloaded);
    terminalHost.dataset.aggregateSessionUploaded = String(demo.aggregateSessionUploaded);
    terminalHost.dataset.simulationElapsedSeconds = String(demo.simulationElapsedSeconds);
    terminalHost.dataset.diskHealthStateLevel = String(demo.diskHealthStateLevel);
    terminalHost.dataset.dhtActiveQueries = String(demo.dhtActiveQueries);
    terminalHost.dataset.dhtPeersFound = String(demo.dhtPeersFound);
    terminalHost.dataset.dhtQueryLoad = String(demo.dhtQueryLoad);
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

    const animationGap = lastAnimationAt === 0 ? FRAME_INTERVAL_MS : now - lastAnimationAt;
    lastAnimationAt = now;
    const targetIntervalMs = 1_000 / Math.max(0.25, demo.targetFps);
    if (animationGap > BACKGROUND_JUMP_MS) {
      lastSimulationAt = now - FRAME_INTERVAL_MS;
      needsFullRefresh = true;
    }
    const elapsed = now - lastSimulationAt;
    const cadenceDue = elapsed >= Math.max(targetIntervalMs - 2, targetIntervalMs * 0.75);

    if (
      document.visibilityState === "visible" &&
      cadenceDue &&
      pendingOperations === 0
    ) {
      const simulationDelta = Math.min(elapsed / 1000, 30);
      demo.advanceSimulation(simulationDelta);
      simulationTickCount += 1;
      renderRequested = true;
      lastSimulationAt = now;
      if (now - lastDiagnosticsAt >= DIAGNOSTIC_INTERVAL_MS) {
        updateDiagnostics();
        lastDiagnosticsAt = now;
      } else {
        updateFrameDiagnostics();
      }
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
    pendingFitSource = source;
    if (immediateFitAnimationFrameId === 0) {
      immediateFitAnimationFrameId = window.requestAnimationFrame(() => {
        immediateFitAnimationFrameId = 0;
        fitTerminal(`${pendingFitSource}:immediate`);
      });
    }
    window.clearTimeout(settledFitTimer);
    settledFitTimer = window.setTimeout(() => {
      settledFitTimer = undefined;
      fitTerminal(`${pendingFitSource}:settled`);
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
      () => {
        flushTimer = undefined;
        enqueueInput(async () => {
          await demo.flushInput();
          terminalHost.dataset.inputFlushCount = String(Number(terminalHost.dataset.inputFlushCount ?? 0) + 1);
        });
      },
      PASTE_BURST_FLUSH_MS,
    );
  };

  const clearEditableTextArtifacts = (): void => {
    for (const node of Array.from(terminalHost.childNodes)) {
      if (node.nodeType === Node.TEXT_NODE) node.remove();
    }
  };
  terminalHost.addEventListener("beforeinput", (event) => event.preventDefault());
  terminalHost.addEventListener("input", clearEditableTextArtifacts);

  document.addEventListener(
    "keydown",
    (event) => {
      if (!terminalHost.contains(document.activeElement)) return;
      if (event.isComposing || event.key === "Dead" || isBrowserShortcut(event) || isModifierOnly(event.key)) return;
      const webQuitCandidate = isWebQuitKey(event, true);
      const recheckWebQuit = webQuitCandidate && inputSequencePending();
      if (webQuitCandidate && demo.webQuitKeyEnabled && !recheckWebQuit) {
        event.preventDefault();
        terminalHost.dataset.webQuitBlockedCount = String(
          Number(terminalHost.dataset.webQuitBlockedCount ?? 0) + 1,
        );
        return;
      }
      event.preventDefault();
      terminalHost.dataset.lastKey = event.key;
      const key = event.key;
      const modifierBits = eventModifiers(event);
      const repeat = event.repeat ? 1 : 0;
      enqueueInput(async () => {
        if (recheckWebQuit) {
          window.clearTimeout(flushTimer);
          flushTimer = undefined;
          await new Promise<void>((resolve) => window.setTimeout(resolve, PASTE_BURST_FLUSH_MS));
          await demo.flushInput();
          if (demo.webQuitKeyEnabled) {
            terminalHost.dataset.webQuitBlockedCount = String(
              Number(terminalHost.dataset.webQuitBlockedCount ?? 0) + 1,
            );
            return;
          }
        }
        const handled = await demo.dispatchKey(key, modifierBits, repeat);
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
      if (event.isComposing || event.key === "Dead" || isBrowserShortcut(event) || isModifierOnly(event.key)) return;
      if (isWebQuitKey(event, demo.webQuitKeyEnabled && !inputSequencePending())) {
        event.preventDefault();
        return;
      }
      event.preventDefault();
      const key = event.key;
      const modifierBits = eventModifiers(event);
      const webQuitCandidate = isWebQuitKey(event, true);
      enqueueInput(async () => {
        if (webQuitCandidate && demo.webQuitKeyEnabled) return;
        await demo.dispatchKey(key, modifierBits, 2);
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
      enqueueInput(() => demo.dispatchPaste(text));
    },
    { capture: true },
  );
  document.addEventListener(
    "compositionend",
    (event) => {
      if (!terminalHost.contains(document.activeElement) || event.data.length === 0) return;
      terminalHost.dataset.lastComposition = event.data;
      clearEditableTextArtifacts();
      queueMicrotask(clearEditableTextArtifacts);
      enqueueInput(async () => {
        await demo.dispatchText(event.data);
        terminalHost.dataset.textCommitCount = String(
          Number(terminalHost.dataset.textCommitCount ?? 0) + 1,
        );
      });
    },
    { capture: true },
  );
  terminalFrame.addEventListener("click", (event) => {
    if (usesTouchKeyboard || (event.target as Element).closest("a")) return;
    queueMicrotask(() => terminal.focus());
  });
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
      lastAnimationAt = lastSimulationAt;
      needsFullRefresh = true;
      renderRequested = true;
      scheduleFit();
    }
  });
  window.addEventListener("pageshow", () => {
    running = true;
    lastSimulationAt = performance.now();
    lastAnimationAt = lastSimulationAt;
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
    if (immediateFitAnimationFrameId !== 0) {
      window.cancelAnimationFrame(immediateFitAnimationFrameId);
    }
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
  lastDiagnosticsAt = performance.now();
  lastSimulationAt = lastDiagnosticsAt;
  lastAnimationAt = lastDiagnosticsAt;
  if (!usesTouchKeyboard) terminal.focus();
  startAnimation();
}

function eventModifiers(event: KeyboardEvent): number {
  const altGraph = event.getModifierState("AltGraph");
  return (event.shiftKey ? 1 : 0) |
    (event.ctrlKey && !altGraph ? 2 : 0) |
    (event.altKey && !altGraph ? 4 : 0) |
    (event.metaKey ? 8 : 0);
}

function rendererDevicePixelRatio(terminal: Terminal): number {
  return mutableDevicePixelRatioRenderer(terminal)?.devicePixelRatio ?? window.devicePixelRatio;
}

function installFixedCellDirtyRowRenderer(terminal: Terminal): void {
  const renderer = terminal.renderer as unknown as FixedCellCanvasRenderer | undefined;
  if (renderer === undefined || typeof renderer.renderLine !== "function") {
    throw new Error("ghostty-web 0.4 fixed-cell renderer contract is unavailable");
  }

  const render = renderer.render.bind(renderer);
  const renderLine = renderer.renderLine.bind(renderer);
  const renderCellText = renderer.renderCellText.bind(renderer);
  let dirtyRows: Set<number> | undefined;

  renderer.renderCellText = (cell, column, row): void => {
    const blank = (cell.codepoint === 0 || cell.codepoint === 32) && cell.grapheme_len === 0;
    const decorated =
      (cell.flags & (CellFlags.UNDERLINE | CellFlags.STRIKETHROUGH)) !== 0 ||
      cell.hyperlink_id > 0 ||
      renderer.hoveredLinkRange != null;
    if (!blank || decorated) renderCellText(cell, column, row);
  };

  renderer.renderLine = (line, row, columns): void => {
    if (dirtyRows === undefined || dirtyRows.has(row)) renderLine(line, row, columns);
  };
  renderer.render = (
    buffer,
    forceAll = false,
    viewportY = 0,
    scrollbackProvider,
    scrollbarOpacity = 1,
  ): void => {
    const cursor = buffer.getCursor();
    const dimensions = buffer.getDimensions();
    const selectionRows = renderer.selectionManager?.getDirtySelectionRows();
    const cursorMoved =
      cursor.x !== renderer.lastCursorPosition.x || cursor.y !== renderer.lastCursorPosition.y;
    const backingSizeChanged =
      renderer.canvas.width !==
        dimensions.cols * renderer.metrics.width * renderer.devicePixelRatio ||
      renderer.canvas.height !==
        dimensions.rows * renderer.metrics.height * renderer.devicePixelRatio;
    const preserveNeighborRows =
      forceAll ||
      backingSizeChanged ||
      viewportY !== 0 ||
      buffer.needsFullRedraw?.() === true ||
      renderer.cursorBlink ||
      renderer.selectionManager?.hasSelection() === true ||
      renderer.hoveredHyperlinkId !== 0 ||
      renderer.previousHoveredHyperlinkId !== 0 ||
      renderer.hoveredLinkRange != null ||
      renderer.previousHoveredLinkRange != null;

    if (!preserveNeighborRows) {
      dirtyRows = new Set<number>();
      for (let row = 0; row < dimensions.rows; row += 1) {
        if (buffer.isRowDirty(row)) dirtyRows.add(row);
      }
      for (const row of selectionRows ?? []) dirtyRows.add(row);
      if (cursorMoved) {
        dirtyRows.add(cursor.y);
        dirtyRows.add(renderer.lastCursorPosition.y);
      }
    }

    try {
      // ghostty-web 0.4 expands every dirty row to both neighbors for combining glyph safety.
      // Superseedr's browser TUI emits fixed-cell terminal symbols, so filtering those redundant
      // neighbors avoids repainting thousands of unchanged small cells while retaining every
      // genuinely dirty row. Complex interaction states conservatively use the upstream path.
      render(buffer, forceAll, viewportY, scrollbackProvider, scrollbarOpacity);
    } finally {
      dirtyRows = undefined;
    }
  };
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
  // Update the ratio without resizing here: its next synchronous render detects the backing-size
  // mismatch, resizes, and paints in the same browser task. Resizing eagerly would clear the
  // visible canvas until the next animation frame and produce a flash during browser zoom.
  renderer.devicePixelRatio = nextDevicePixelRatio;
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

function isBrowserShortcut(event: KeyboardEvent): boolean {
  if (event.getModifierState("AltGraph")) return false;
  if (event.metaKey) return true;
  if (event.shiftKey && event.key === "Insert") return true;
  if (event.ctrlKey && ["-", "+", "=", "0"].includes(event.key)) return true;
  if (event.ctrlKey && event.key.toLowerCase() === "v") return true;
  return event.ctrlKey && event.key.toLowerCase() === "c";
}

function isWebQuitKey(event: KeyboardEvent, enabled: boolean): boolean {
  return enabled && event.key === "Q" && !event.ctrlKey && !event.altKey && !event.metaKey;
}

function requireElement<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!(element instanceof HTMLElement)) throw new Error(`missing #${id}`);
  return element as T;
}

function requireSelector<T extends HTMLElement>(selector: string): T {
  const element = document.querySelector(selector);
  if (!(element instanceof HTMLElement)) throw new Error(`missing ${selector}`);
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
