# Changelog

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
