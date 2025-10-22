# superseedr - A BitTorrent Client in your Terminal

BitTorrent client written fully in Rust using **[Ratatui](https://ratatui.rs/)**.

![Feature Demo](https://github.com/Jagalite/superseedr-assets/blob/main/superseedr_landing.webp)

## Installation pre-release
Install using cargo:
```bash
cargo install superseedr
```
Launch TUI (Terminal UI) + BitTorrent Client
```bash
superseedr
```
## Preview only / Tested on M1 Mac / kitty and Ghostty
Once running, add torrents by pasting (`ctrl+v`) a magnet link or path to a `.torrent` file. 

You can also add torrents or magnet links via another terminal command line while the TUI is running (make sure to set a download path first):
```bash
# Add a magnet link to the running instance
superseedr "magnet:?xt=urn:btih:..."

# Add a local torrent file path to the running instance
superseedr "/absolute/path/to/my.torrent"

# Stop the running application instance
superseedr stop-client
```
## Build and Run
Clone the project and run the application directly using Cargo:
```bash
cargo run
```
Configuration files are located in the user's Application Support folder:
`Press [m] in the tui to see log and config path`

## Current Status & Features

The client is in a late-alpha stage, with most core BitTorrent features implemented and functional.

### Core Protocol & Peer Discovery
- **Real Time Performance Tuning:** Periodic resource optimizations (file handles) to maximize speeds and disk stability.
- **Peer Discovery:** Full support for Trackers, DHT, PEX, and Magnet Links (including metadata download).
- **Piece Selection:** Utilizes a Rarest-First strategy for optimal swarm health, switching to Endgame Mode for the final pieces.
- **Choking Algorithm:** Employs a tit-for-tat based choking algorithm with optimistic unchoking for efficient upload slot management.

### User Interface (TUI)
- **Real-time Dashboard:** A `ratatui`-based terminal UI displaying overall status, individual torrent progress, peer lists, and network graphs.
- **Help & Commands:** A help popup lists all keyboard commands, and a footer bar shows common commands.

### Configuration & Management
- **Persistent State:** Saves the torrent list, progress, and lifetime stats to a configuration file.
- **Speed Limits:** Allows setting global upload and download speed limits.

## Roadmap to V1.0
- **Testing:** Ongoing testing across various platforms and terminals.
- **Magnet Link Handling:** Implementation of operating system-level integration (e.g., registry/app associations) for seamless browser-to-app magnet link capture.
- **CI/CD:** Implementation of a full CI/CD pipeline.
- **Build & Distribution:** Work in progress for streamlined building and distribution.
- **Unit Testing:** Expansion of unit test coverage.

## Roadmap to V1.5
- Fix and refactor synchronous startup and validation

## Future (V2.0 and Beyond)
- **Network History:** Persisting network history to disk.
- **RSS Support:** Integration of RSS feed support.
- **Headless Mode:** A headless mode alongside the TUI.
- **Torrent Log book:** Historic log book of all torrents added and deleted. Allows users to search and redownload.
