# Changelog

## Release v1.0.13
### 🚀 New Features
- **Torrent Tags**: Added persistent manual tags with stable IDs, shared-catalog assignments, a themed `L` tag-filter workspace with flowing pills and torrent/file results, an inline `t` tag picker shared with Torrent Management for bulk assignment and create-and-assign, and matching text/JSON CLI operations for creating, renaming, deleting, assigning, removing, listing, and inspecting tags.
- **Global Peer Management**: Added a dedicated `P` workspace for inspecting peers across all torrents, with activity and restriction filters, fuzzy or regex search, sortable columns, detailed evidence views, and optional privacy masking.
- **Automatic Malicious-Peer Protection**: Added a global policy that detects excessive transfer activity and rapid reconnect churn, temporarily restricts offending addresses, and carries those protections across active torrents and restarts.

### ✨ Improvements
- **Event Journal Activity View**: Redesigned the journal around grouped ingest and command lifecycles, clearer status timelines and detail summaries, responsive paging, and search across control targets, health details, paths, hosts, and messages.
- **Help Workspace**: Reorganized Help into a responsive command index with clearer section navigation, descriptions, contextual styling, and layouts that keep search, warnings, content, and controls usable in compact terminals.
- **Reactive Peer Telemetry**: Replaced peer-metrics polling with shared notifications so peer evidence and policy views update promptly while avoiding repeated full-state work when activity is unchanged.

### 🐛 Bug Fixes
- **Peer Policy Enforcement**: Restricted peers are now rejected consistently on inbound and outbound paths, and newly restricted active sessions are removed even when their command channels are busy.
- **Peer Evidence Reliability**: Preserved final transfer totals and reconnect evidence when peers disconnect or torrents stop, normalized equivalent IP representations, and hardened policy persistence and recovery.
- **Journal and Help Navigation**: Fixed journal search coverage, selection and page-scroll clamping, compact detail behavior, and Help warning visibility across narrow and short terminal layouts.

## Release v1.0.12
### New Features
- **Redesigned Configuration Workspace**: Rebuilt the TUI configuration screen with grouped settings, responsive master-detail layouts, setting-specific guidance, exact-value editing, and clearer immediate-apply controls.
- **Safe Offline Torrent Moves**: Added `superseedr move <INFO_HASH> <PATH>` for relocating torrent payloads while the client is stopped, including same-volume moves and cross-volume copy support.
- **Inbound Transport Reachability**: Added TCP and uTP reachability visibility to the listen-port details so operators can see which protocol and address-family paths have accepted inbound peers.
- **Expanded Linux Release Artifacts**: Added amd64 and arm64 Debian packages and GNU tarballs for normal and private builds, including checksums and install smoke-test coverage.

### Improvements
- **Torrent Management Review Workflow**: Redesigned the batch-action review modal with responsive layouts, scrollable large-batch summaries, live purge-size totals, clearer destructive-action guidance, and more predictable staged-action replacement and clearing.
- **File Browser and Torrent Preview UX**: Improved search-aware cursor recovery, asynchronous fetch handling, visible error reporting, cached torrent previews, case-insensitive torrent-file selection, and navigation around filtered or delayed results.
- **macOS Protocol Handler Diagnostics**: Replaced the generated script handler with a signed universal native handler, added registration verification and retry behavior, and introduced privacy-safe installer and handler logs for troubleshooting.
- **Runtime and Release Maintenance**: Refreshed core dependencies, corrected the Solarized theme palettes, omitted direct-input logging from private builds, and added a comprehensive release-candidate validation runbook.

### Bug Fixes
- **Move Transaction Safety**: Hardened offline moves with destination-conflict and free-space preflights, staged cross-volume copies, metadata verification, rollback handling, and recoverable behavior when configuration persistence fails.
- **Multi-File and v2 Storage Correctness**: Fixed torrent path resolution, file-tree and priority edge cases, storage allocation behavior, and v2/hybrid padding handling across preview, move, and management workflows.
- **Configuration Input Consistency**: Standardized `Space` as the primary setting action, removed accidental pane switching through `Tab`, constrained controls to compatible setting types, and tightened listen-port validation.
- **macOS Package Registration Reliability**: Fixed handler signing and packaging, empty build-flag handling, startup registration races, and default-handler verification after installation.
- **Shared Status Scheduling**: Kept the status snapshot timer enabled at a five-second default for both shared leaders and followers when no explicit output interval is configured, while preserving disabled snapshots in standalone mode.

## Release v1.0.11
### New Features
- **Configurable TUI Layout Modes**: Added a `ui_layout_mode` setting, with `layout` accepted as an alias, so the main TUI can stay in automatic mode or be forced into vertical or horizontal layouts.
- **Always-Show Add Location Prompt**: Added `always_show_add_location_prompt` so magnet and torrent adds can consistently open the location browser instead of silently using a default path, while preserving shared-mode fast paths where they are still required.
- **Torrent List Page Navigation**: Added page-up/page-down style movement for torrent and peer lists so longer sessions are easier to scan from the keyboard.

### Improvements
- **Responsive Torrent Table Columns**: Sized torrent-list speed columns dynamically so longer units such as Mbps remain visible instead of being clipped in tighter layouts.
- **Add Browser State Handling**: Improved manual add and pending magnet browser flows so prompt state survives metadata hydration, stale preview state is replaced cleanly, and pending entries are not editable while their real metadata is still loading.
- **Shared Add Flow Consistency**: Routed shared follower manual magnets through the leader, kept follower-local prompt state from leaking into shared adds, and preserved queued follower paths when the leader applies the request.

### Bug Fixes
- **Pending Magnet Preview Finalization**: Fixed direct magnet and torrent-file duplicates for the same info hash so they apply to an open pending preview, clear the pending-preview marker, and persist as real torrents instead of being removed by a later cancel.
- **Deferred Ingest Archive Correctness**: Keyed deferred shared-inbox archives to the submitted manual add, cleared stale deferred records when prompts are replaced or fail, and retired ingest journal entries after successful deferred archive.
- **Fictional Test Fixtures**: Updated torrent fixture names used by tests to avoid real media titles while keeping the coverage shape intact.

## Release v1.0.10
### New Features
- **Torrent Management Workspace**: Added a dedicated torrent-management screen for reviewing active torrents, searching/filtering the list, staging batch pause/resume/delete actions, and confirming changes before they are applied.
- **Interactive Existing-Torrent Editing**: Existing torrents can now be opened into the file browser for priority review/editing, including per-file priority changes, bulk priority cycling, and clearer preview-only navigation.
- **Browser Search and Preview Controls**: Added searchable torrent previews, pane-aware browser search, expand/collapse-all controls, and clearer footer hints for priority, expand, collapse, and confirmation actions.

### Improvements
- **File Browser Workflow Polish**: Improved add-torrent and existing-torrent browser flows with better starting locations, stale file-tree response protection, preview-focused existing-torrent handling, and safer generation tracking for asynchronous file-tree loads.
- **Help and Action Styling Refresh**: Reworked the help menu and footer/action styling with clearer grouping, scroll clamping, themed action tones, and more consistent shortcut descriptions across screens.
- **TUI Reliability and State Persistence**: Persisted UI refresh-rate settings, improved torrent search behavior, clarified rate-sort arrows, and queued TUI app commands until shutdown so late UI work is handled predictably.
- **Runtime and Dependency Maintenance**: Upgraded Ratatui, refreshed lockfile dependencies, tightened synthetic-load/anonymized-label behavior, and removed obsolete peer-stream prototype code.

### Bug Fixes
- **Torrent File Priority Accuracy**: Fixed torrent-preview folder priority aggregation so only folders with genuinely mixed child priorities are marked mixed; unrelated folders now keep their correct priority state.
- **Filesystem and Allocation Safety**: Hardened mounted-filesystem writes and avoided stale file metadata during allocation, reducing incorrect size/path handling on mounted or changing storage.
- **Test and Shutdown Stability**: Removed flaky wall-clock token-bucket assertions, tamed transfer performance tests, and made public-tracker stop announces fire-and-forget so shutdown is less likely to block on tracker latency.

## Release v1.0.8
### New Features
- **TCP + uTP Peer Transport**: Enabled production peer transport over both TCP and uTP by default, with TCP fallback when uTP is slow or unreachable.
- **Per-User Windows Installer**: Added a no-admin Windows installer path with user-level PATH setup, Start Menu shortcuts, and magnet and `.torrent` file associations.
- **Transport Validation Tooling**: Added transport-specific benchmark modes, deterministic uTP chaos controls, fuzz harnesses, and interoperability lab coverage to exercise peer behavior under stress.

### Improvements
- **uTP Stability and Throughput**: Tuned congestion control, ACK behavior, outbound connection handling, and shared UDP routing for more reliable uTP sessions.
- **Automatic Limit Visibility**: The TUI now highlights effective automatic download caps so disk backpressure and self-tuning behavior are easier to understand at a glance.
- **Benchmark and CLI Controls**: Documented runtime peer transport controls and added benchmark transport selection so operators can compare TCP, uTP, and mixed runs directly.

### Bug Fixes
- **Disk Backpressure Recovery**: Cleared stale disk-pressure throttles and pending write-latency state after failed writes so old signals do not keep limiting downloads.
- **Transport Fallback Reliability**: Hardened TCP/uTP parity, outbound uTP congestion handling, and mixed-transport connection paths so peers recover more predictably from degraded routes.
- **Logging Fallback Handling**: Improved file-logging setup fallback and runtime error reporting so diagnostics remain available when log files cannot be opened.

## Release v1.0.7
### New Features
- **Synthetic Benchmark Harness**: Added feature-gated benchmark tooling for local download, upload, and swarm capacity testing, with adaptive step sizing, JSON summaries, ETA reporting, and generated run artifacts; see [`docs/synthetic-benchmark.md`](synthetic-benchmark.md).
- **Adaptive Disk Backpressure**: Added live download throttling that reacts to disk write pressure so high-throughput sessions can back off when storage latency becomes the bottleneck.
- **Additional Watch Paths**: Restored support for additional watch folders so operators can ingest torrents from multiple configured locations.

### Improvements
- **TUI Swarm Visibility**: Refined the peer table, files panel, and swarm heatmap with inactive-peer summaries, saturated-peer file strips, more compact peer-action labels, and better behavior in short terminal layouts.
- **Performance Diagnostics**: Added measured footer FPS, disk-queue write latency timing, clearer benchmark capacity reporting, and better synthetic peer ramping for load investigations.
- **Docker and Client Interop**: Made the Docker Compose setup wait for Gluetun health, grouped interop lanes by client, and improved qBittorrent login and add-response compatibility.
- **Dependency and Runtime Maintenance**: Removed config/logging helper dependencies, hardened the replacement config, logging, and atomic-write paths, refreshed dependency pins, and kept CI on the released Rust 1.95.0 toolchain.

### Bug Fixes
- **Piece Request Correctness**: Fixed duplicate requests during piece writes, adjusted endgame request scheduling, and removed defensive pending-block scheduling that could interfere with active work.
- **Telemetry and Logging Accuracy**: Fixed logging flush behavior and telemetry percentile regressions so runtime diagnostics stay reliable under load.
- **Layout Edge Cases**: Fixed files-panel and heatmap sizing edge cases in constrained terminal space while preserving useful peer and availability context.
- **Synthetic Load Stability**: Fixed synthetic-load permit accounting and Windows clippy issues in the benchmark-only build path.

## Release v1.0.6
### 🚀 New Features
- **Internal DHT Peer Discovery**: Added an internal DHT backend with routing-cache support, peer announce support, dual-stack discovery hints, and health data in status snapshots.
- **DHT Activity Visibility**: Added DHT wave and peer-yield visibility in the TUI so discovery behavior is easier to monitor while torrents are running.
- **Configuration Path Reporting**: Added effective config path reporting to make standalone and shared runtime locations easier to verify.

### ✨ Improvements
- **Smarter Peer Discovery Scheduling**: Improved DHT demand planning, fairness, no-peer backoff, crawl reuse, and peer-slot pressure handling so discovery work stays useful without overpowering active peer traffic.
- **More Reliable DHT Startup and Reconfiguration**: DHT startup now falls back more safely when a runtime cannot bind, and port reconfigure/rebind paths release old sockets more predictably.
- **Quieter Runtime Logs**: Reduced repeated startup and shared-config logging, removed noisy metrics journaling logs, and kept concise startup/revision signals for operators.
- **TUI Usability Polish**: Restored vim-style right navigation, fixed dynamic table sorting, refined autosort pin behavior, and simplified the footer FPS display.

### 🐛 Bug Fixes
- **Skipped File Safety**: Existing skipped files are no longer resized during allocation, protecting user-skipped data from unwanted truncation or expansion.
- **DHT Recovery Correctness**: Fixed stale empty lookups, failed rebind handling, reset peer-slot pressure resend, finished lookup handles, and metadata edge cases.
- **DHT Status Accuracy**: Disabled or no-runtime DHT status now preserves configured bootstrap counts and reports responsive bootstrap nodes uniquely by address family.
- **Protocol and Catalog Hardening**: Hardened DHT KRPC decoding, public identity selection, announce-token behavior under pressure, and catalog recovery from torrent contents.

## Release v1.0.5
### 🚀 New Features
- **Torrent Files Panel**: Added a dedicated files view so you can inspect a torrent’s file tree, follow cleaner relative paths, and watch live per-file activity as downloads and uploads run.
- **Broader Tracker Coverage**: Added UDP tracker support and stronger IPv6 handling, helping superseedr connect to more swarms and work better on modern network setups.

### ✨ Improvements
- **Smoother Startup**: Torrents now roll in gradually during startup, reducing bursty load and making launches calmer and easier to follow.
- **Clearer Live Monitoring**: The peer table now presents addresses and activity more clearly, and multi-torrent views rank by current smoothed traffic for a more useful at-a-glance overview.
- **Better Path and Panel Feedback**: File-path shaping, shared inbox path handling, and file-panel activity rendering are more consistent across local and shared environments.

### 🐛 Bug Fixes
- **Pause Reliability**: Pausing a torrent now stops peer activity more reliably so downloads do not keep talking to connected peers after pause.
- **Tracker URL Handling**: Fixed tracker normalization so equivalent tracker URLs are handled more safely, reducing duplicate or mis-prioritized announce behavior.
- **Files Panel Refresh**: Fixed stale or inconsistent file-activity updates so the files view stays in sync more reliably during startup and runtime refreshes.

## Release v1.0.4
### New Features
- **Shared Configurations & Cluster Mode**: Added layered shared configuration with leader/follower clustering, automatic failover, host-local runtime folders under `hosts/<host-id>/`, and shared desired state that can be reused across machines and operating systems.
- **Launcher-Persistent Shared Setup**: Added launcher-side shared configuration commands so installed app and protocol launches can resolve shared mode and host identity without relying on shell environment variables.
- **Expanded Shared CLI Surface**: Added `set-shared-config`, `clear-shared-config`, `show-shared-config`, `set-host-id`, `clear-host-id`, `show-host-id`, `to-shared`, `to-standalone`, `torrents`, `info`, and `files`.

### Improvements
- **Remove/Purge CLI Model**: Reworked deletion controls into `remove` and `purge`, added offline-capable purge behavior when file layout is resolvable, and unified reverse file-path lookup across hash-targeted CLI commands.
- **Cross-Host Shared Ingest**: Shared-mode `.path` adds now use portable shared-root-relative handling so staged torrent ingest can work across mixed-OS clusters instead of leaking host-local absolute paths.
- **Shared Runtime UX & Observability**: Improved shared journal/status semantics, added JSON envelope support across CLI commands, made journal human-readable by default, and clarified shared-mode startup and mount validation behavior.

### Bug Fixes
- **CLI Startup Robustness**: Fixed shared-mode CLI startup so logging failures fall back safely instead of blocking command handling, and improved mount/accessibility errors for missing or unavailable shared roots.
- **Offline Shared Mutation Routing**: Fixed shared-mode offline CLI control so commands mutate shared config directly when no leader is running instead of only queueing requests.
- **Welcome Screen Paste Handling**: Fixed explicit paste behavior so the welcome screen no longer routes pasted torrent input through the normal-mode paste handler.
## Release v1.0.3
### 🚀 New Features
- **Persistent Activity History Charts**: Added on-disk activity history for per-torrent and system activity charts, so chart views can restore meaningful trend data after restart.
- **Expanded Activity Chart Views**: Added richer chart panels for CPU, RAM, disk activity, tuning, per-torrent overlays, and multi-torrent comparisons, with longer-range history views for deeper monitoring.
- **Torrent Data Availability Probing**: Added active data-availability checks that can detect missing or inaccessible torrent data and surface the issue directly in the interface.

### ✨ Improvements
- **Integrity Recovery Scheduling**: Added a dedicated integrity probe scheduler so completed and active torrents are rechecked more predictably without overloading normal download activity.
- **Chart Readability and Navigation**: Refined chart rendering with clearer line-based traffic views, better speed scaling, and smoother switching between overlay modes and graph panels.
- **Connection Resilience**: Hardened peer handling with inbound handshake timeouts and protection against abusive message floods, helping sessions recover more cleanly under hostile or stalled network conditions.
- **Probe UI Clarity**: Improved the torrent details and probe display so data-availability issues and follow-up checks are easier to understand at a glance.

### 🐛 Bug Fixes
- **Per-Torrent Activity History Preservation**: Torrent activity charts now keep their history intact while you search or filter the list, so hidden active torrents do not lose their recorded trend data.
- **Saved-Location Integrity Recovery**: File-availability faults now trigger immediate re-checks only for the affected saved location, preventing unrelated torrents in the same download root from being unnecessarily reprobed.

## Release v1.0.2
### 🚀 New Features
- **Persistent Network History**: Added on-disk network history so charts can restore download, upload, and backoff data after restart.
- **Extended Network Graph Ranges**: Added 3h, 12h, 24h, 7d, 30d, and 1y graph views for longer-range traffic visibility.
- **New Particle Themes**: Added Sakura and Matrix themes, plus layered particle rendering for richer animated backgrounds.

### ✨ Improvements
- **Adaptive Self-Tuning**: Self-tuning now starts faster and adjusts its cadence more intelligently when conditions improve, stall, or regress.
- **History Persistence Efficiency**: Network history snapshots are now more compact and restore more predictably after restart.
- **Theme Motion Polish**: Particle timing, swarm heatmaps, and motion profiles were refined across Black Hole, Diamond, and Bioluminescent Reef.

### 🐛 Bug Fixes
- **Graph Window Alignment**: Fixed long-range network charts so minute, 15-minute, and hourly buckets align to wall-clock boundaries instead of rendering shifted windows.
- **Network History Recovery**: Fixed restore and densification edge cases that could leave persisted history incomplete or misaligned after startup.
- **Deletion Restart Behavior**: Torrents already marked for deletion no longer reappear in the UI after an interrupted shutdown and restart.
- **Theme Rendering Stability**: Fixed Black Hole aspect-scaling issues and improved disk panel readability during particle-heavy themes.

## Release v1.0.1
### 🚀 New Features
- **Duplicate Filter Guardrails**: RSS filter creation now blocks duplicates for the same mode using normalized matching (trim + case-insensitive), with a clear in-app status message.

### ✨ Improvements
- **First-Filter Draft Highlighting**: While creating the first RSS filter, Explorer match highlighting now remains visible instead of being fully greyed out.
- **History Row Readability**: RSS History rows now render source at the end of each line for better title-first scanning.
- **RSS Add-Entry Shortcut Consistency**: Restored `[a]` add-entry behavior for Links and Filters panes to match expected key-hint behavior.

### 🐛 Bug Fixes
- **Filter List Selection Safety**: Fixed filter-pane selection clamping to use rendered item count, preventing stale-index edge cases.
- **RSS Interaction Regressions Covered**: Added/updated RSS tests for link/filter add-entry keyflow, duplicate filter rejection, and first-filter draft highlighting behavior.

## Release v1.0.0
### 🚀 New Features
- **Integrated RSS Workspace**: Added a full RSS experience in-app, including feed management, preview browsing, and streamlined ingest so you can discover and add downloads without leaving the terminal.
- **Advanced RSS Filtering**: Added richer filter modes and smarter match handling so you can surface relevant feed items faster and reduce manual triage.
- **Safer High-Impact Actions**: Added stronger confirmation flows (including Shift+Y confirmation paths) to reduce accidental destructive actions during everyday use.

### ✨ Improvements
- **Magnet Name Resolution**: Magnet entries now resolve display names more reliably from link metadata, making startup and history views clearer.
- **UI Flow Consistency**: Screen transitions, search behavior, and key routing are more predictable across browser, normal, and help flows for smoother navigation.
- **Persistence Robustness**: Torrent metadata persistence is now more durable across restart scenarios, with safer file finalization behavior on platform-specific edge cases.

### 🐛 Bug Fixes
- **DHT Recovery Handling**: Fixed DHT rebind/bootstrap warning handling and added automatic recovery retries so temporary bootstrap failures are less disruptive.
- **Progress Accuracy with Skipped Files**: Fixed completion/progress display for torrents with skipped files to avoid misleading partial-progress states.
- **RSS Network Safety Edge Cases**: Fixed RSS URL safety handling for localhost/private-network edge cases (including IPv6-localhost parsing) and improved stale RSS error cleanup.

## Release v0.9.39
### 🚀 New Features
- **Boundary-Aware Piece Scheduling**: Added a piece-local request path that correctly handles torrents where piece sizes are not aligned to 16 KiB blocks, improving reliability on edge-case torrent layouts.

### ✨ Improvements
- **Smarter Download Request Routing**: Request and cancel generation now use piece-local block tuples, making peer work assignment more consistent after resume/restart and during multi-peer scheduling.
- **More Predictable Completion Flow**: Piece/block coordination is now cleaner in non-aligned layouts, reducing false “in-flight but no progress” behavior under heavy swarm traffic.

### 🐛 Bug Fixes
- **Non-Aligned Torrent Stalls**: Fixed a bug where downloads could appear active but stop advancing because needed boundary blocks were incorrectly suppressed.
- **Piece Boundary Handling**: Fixed edge cases where adjacent pieces sharing a global block slot could interfere with each other’s progress.
- **Resume Verification Consistency**: Improved restart/resume behavior so verified progress and subsequent block requests stay aligned with actual missing data.

## Release v0.9.38
### 🚀 New Features
- **Richer Built-In Theme System**: Added a semantic theme engine plus multiple new themes (including Neon and Candy Land Pink) for deeper, more consistent UI personalization.
- **One-Key Theme Cycling**: You can now switch themes instantly from the keyboard, without leaving your workflow.
- **Global Theme Effects**: Visual effects now apply across more of the interface, including swarm and stream styling, for a more cohesive live UI experience.

### ✨ Improvements
- **Smarter Footer Readability**: Footer and command hints now fit terminal width more intelligently, with cleaner truncation and stable theme/effects labels.
- **More Reliable Table Navigation**: Torrent/peer column visibility, sorting, and header navigation are now synchronized for predictable behavior in dynamic layouts.
- **Higher Peer Capacity and Clearer Status**: Connection admission limits were raised and activity messaging was improved, helping large swarms stay clearer and more stable.
- **More Flexible Rate Shortcuts**: Data-rate controls now accept both `[`/`]` and `{`/`}` for easier use across keyboard layouts.

### 🐛 Bug Fixes
- **Skip-Hashing Validation Accuracy**: Skip-hashing no longer marks torrents as validated when required files are missing or incomplete on disk.
- **Validation State Correctness**: Metadata-transition edge cases now preserve prior valid state correctly and downgrade it when completion regresses.
- **Theme and Layout Sync Fixes**: Fixed cases where theme/config sync and hidden-column handling could drift, plus footer cutoff issues on narrower screens.

## Release v0.9.37

- **Version bump:** fixing github with new macOS flow.

## Release v0.9.36

### 🚀 New Features
- **Smart First-Run Setup**: On first launch, the app now automatically detects your system's Downloads folder and configures it as the default download location—no manual setup required.
- **Intelligent Welcome Screen**: The welcome screen now only appears for truly new users and automatically dismisses when you add your first torrent.

### ✨ Improvements
- **Enhanced Peer Activity Visualization**: Redesigned the peer stream display with improved visual density—Braille-style dots for light activity and emphasized markers for heavy peer connections, making it easier to spot swarm health at a glance.
- **Watch Path Visibility**: The configured watch folder path is now displayed in the interface for better transparency.
- **Smart Progress Column**: The progress column in the torrent list will only show when torrents are downloading to validating.

### 🐛 Bug Fixes
- None in this release.

## Release v0.9.35
### Performance
- Added periodic application state dump to JSON for external monitoring/integrations.
- Configured rolling file appender for logs with daily rotation and 31-day retention.

### Refactoring
- Modularized integration logic into a new `src/integrations/` directory.
- Decoupled CLI argument parsing and input processing into `src/integrations/cli.rs`.
- Externalized file system watching and folder scanning logic into `src/integrations/watcher.rs`.
- Centralized application status serialization and export in `src/integrations/status.rs`.
- Simplified `App` struct by delegating file event handling and watch folder scanning to the integrations module.

### Testing
- Added unit tests for CLI input processing, including magnet link and path file handling.
- Added unit tests for file watcher logic and command mapping.
- Added serialization tests for the new JSON status dump feature.

## Release v0.9.34
### Performance
- Dynamically hide download/upload speed columns when no activity detected

### Refactoring
- Added `container_name` field to torrent configuration for explicit folder control
- Implemented intelligent container naming: auto-generates folders with info_hash suffix for multi-file torrents
- Added support for explicit "no folder" option to flatten multi-file torrents to single directory

### Testing
- Added unit test for container logic with explicit empty folder selection
### Performance
- Implemented dynamic framerate control based on app mode (60 FPS for Welcome screen, 1 FPS for Power Saving mode, user-defined otherwise)

### Refactoring
- Changed quit key binding from lowercase 'q' to uppercase 'Q' to prevent accidental quits
- Added text sanitization for torrent names and paths to handle control characters gracefully

### Testing

## Release v0.9.32
### Refactoring
- Moved file watcher to App struct for dynamic reconfiguration during runtime.
- Updated GitHub Actions to latest versions (checkout@v6, cache@v5).

### Performance
- Updated dependencies for improved performance and stability.

### Testing
- Updated proptest cases for nightly fuzzing.

## Release v0.9.31
### Performance
- Optimized file allocation by skipping padding and skipped files.
- Added fast-path detection for fresh downloads vs partial resumes.

### Refactoring
- Introduced file priority system (Normal, High, Skip) for per-file download control.
- Implemented tree-based file browser with preview for download location selection.
- Added settings backup system with timestamped archives.
- Changed download path from required to optional, deferring selection until metadata loads.
- Renamed `DhtTorrent` to `MetadataTorrent` for clarity.
- Refactored `download_dir` to `torrent_data_path` across torrent management.

### Testing
- Added tree navigation tests for the new file browser.
- Added storage tests for skipped file handling.

## Release v0.9.30
### Performance
- Optimized BitTorrent v2 verification with small-file root lookup bypassing.
- Implemented memory-aware cleanup logic for v2 pending data buffers.
- Improved piece request pipelining with deterministic rarity-first selection.

### Refactoring
- Introduced BitTorrent v2 and Hybrid torrent support (BEP 52).
- Implemented Merkle tree verification engine for v2 data integrity.
- Refactored torrent parser to handle v2 file trees and synthetic padding files (BEP 47).
- Decoupled piece geometry from contiguous streams to support file-aligned pieces.
- Enhanced TUI with an "Add Torrent" file picker and improved watch folder management.

### Testing
- Added comprehensive v2/hybrid integration tests covering boundary alignment and proof verification.
- Introduced scale tests for 1000-piece torrents to verify pipeline stability.
- Added proptest-based network fault injection for the state machine.

## Release v0.9.29
### Performance
- Introduced "Smart Table" logic to dynamically hide columns based on priority and width.
- Optimized TUI event listener to use non-blocking polls for better shutdown responsiveness.

### Refactoring
- Major TUI refactor: decoupled layout calculation from rendering logic.
- Modularized TUI components into `src/tui/` directory.
- Introduced `LayoutContext` and `LayoutPlan` for structured UI management.

### Testing
- Added unit tests for new TUI navigation logic.
- Enhanced `Settings` parsing tests with comprehensive coverage.

## Release v0.9.28
### Performance
- Implemented a dynamic request window size in `PeerSession` to improve download throughput.
- Optimized `TokenBucket` to reduce lock contention for unlimited rates.
- Improved network writer performance by batching messages to reduce syscalls.

### Refactoring
- Replaced single block requests with a `BulkRequest` system for better pipelining.
- Updated `web_seed_worker` to use the new bulk request system.
- Refactored `TorrentManager` and its state machine to support bulk commands.

### Testing
- Added extensive tests for the new dynamic window sizing logic in `PeerSession`.
- Added a proptest regression file to save and re-run failure cases.


## Release v0.9.27
### Features
- Added block manager to improve download performance.

### Bug Fixes
- Updated torrent sorting weight for better prioritization.
- Added more tests and fixed tolerance issues.

### Refactoring
- Consolidated and adjusted TUI components.
- Added testing and integration via composition.

### Performance
- Increased in-flight request limits for better throughput.


## Release v0.9.26
### Features
- **Advanced Networking**: Implemented `web-seed-workers` for improved seeding, and an "effect pattern" for more resilient network communication. Added network simulations for robust testing.
- **Core Refactoring**: Major refactoring of the codebase for better performance and maintainability, including the implementation of a resource manager and an adaptive seek penalty.
### Bugs
- **Comprehensive Testing**: Introduced a wide range of testing strategies, including chaos engineering, fuzz testing, and state machine-based tests to ensure stability and reliability.

## Initial Features
- **Cross-Platform Support**: Added robust support for major operating systems, including Windows (Wix installer), macOS (notarized builds), and Linux (MUSL builds).
- **Dynamic TUI**: Overhauled the Text User Interface (TUI) with new features like a swarm heatmap, peer activity lanes, and dynamic resizing, providing a more informative and user-friendly experience.
- **Docker Integration**: Full Docker support with examples for docker-compose, multi-architecture builds (ARM), and integrated VPN (Gluetun) support for enhanced privacy.
- **CI/CD Pipeline**: Established a comprehensive CI/CD pipeline using GitHub Actions for automated testing, linting, and releases.
