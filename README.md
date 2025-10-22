# superseedr - A BitTorrent Client in Rust

Terminal-based BitTorrent client written in Rust using **[Ratatui](https://ratatui.rs/)**.

![Feature Demo](https://github.com/Jagalite/superseedr-assets/blob/main/superseedr_landing.webp)

## Installation
Install using cargo:
```bash
cargo install superseedr
```

## Preview / Running Locally / Tested on M1 Macbook
Run the application directly using Cargo:
Loading torrents via Paste (crtl + v) or by setting a torrent watch directory.
```bash
cargo run
```
You can also add torrents or magnet links via the command line while the TUI is running:
```bash
superseedr add "magnet:?xt=urn:btih:..."
```
Configuration files are located in the user's Application Support folder:
`Press [m] in the tui to see log and config path`

## Current Status & Features
The client is currently in a late-alpha stage. Most of the core BitTorrent protocol features are implemented and functional.

-   **Multi-Torrent Support:** Download and seed multiple torrents simultaneously.
-   **Peer Discovery:**
    -   ✅ **Trackers:** Fully supported.
    -   ✅ **DHT (Distributed Hash Table):** Find peers on trackerless torrents.
    -   ✅ **PEX (Peer Exchange):** Discover peers from other connected peers.
    -   ✅ **Magnet Links:** Add torrents using magnet links, with metadata download.
-   **Core Protocol:**
    -   ✅ **Piece Selection:** Implements a Rarest-First strategy for swarm health, transitioning to Endgame Mode for the final pieces.
    -   ✅ **Choking Algorithm:** A tit-for-tat based choking algorithm with optimistic unchoking to efficiently manage upload slots.
    -   ✅ **Partial Downloads:** Resumes downloads from partially completed files.
-   **User Interface (TUI):**
    -   ✅ **Real-time Dashboard:** A terminal UI built with `ratatui` showing overall status, individual torrent progress, peer lists, and network graphs.
    -   ✅ **Torrent Management:** Pause, resume, and delete torrents (with or without deleting files on disk).
    -   ✅ **Interactive Settings:** An in-app screen to view and edit all client settings.
    -   ✅ **Help Screen:** A popup with all available keyboard commands.
    -   ✅ **Footer Commands:** A footer bar showing common commands.
    -   ✅ **Announce Timers:** The TUI now shows a countdown for the next tracker announce.
-   **Configuration:**
    -   ✅ **Persistent State:** Saves torrent list, progress, and lifetime stats to a configuration file.
    -   ✅ **Cross-Platform:** Automatically stores configuration in the appropriate user data directory. 
    -   ✅ **Upload/Download Limits:** Set global upload and download speed limits.

---

## Roadmap to V1.0
- Testing in various platforms and terminals.
- Handling magnet links via registry, app, ...etc
- CI/CD
- Building and distribution WIP.
- Unit testing

## Future (V2.0 and Beyond)
- Persist to disk network history.
- RSS.
- Headless + tui.
- Advance tuning algorithm for disk penalties.