# superseedr - A BitTorrent Client in your Terminal

A **standalone** BitTorrent client created with **[Ratatui](https://ratatui.rs/)**.

It features a custom-built BitTorrent protocol implementation in Rust focused on observability.

![Feature Demo](https://github.com/Jagalite/superseedr-assets/blob/main/superseedr_landing.webp)

## Installation

Find releases for all platforms on the [releases page](https://github.com/Jagalite/superseedr/releases)

Magnet links and torrent files are fully supported with installation.

> [!NOTE]  
> Some terminals start with very low ulimits (256). superseedr can still operate, but consider increasing for maximum performance and stability: `ulimit -n 65536`.

> [!NOTE]  
> macOS's default terminal application does not support truecolor just yet (soon!), try using kitty or Ghostty.

### Private Tracker Builds
This installation is intended for private trackers, as it disables peer-discovery features (DHT & PEX).
These features will not be included in the final build of the private versions of superseedr.

## Usage
Open up a terminal and run:
```bash
superseedr
```
> [!NOTE]  
> Add torrents by clicking on magnet links from the browser and or opening torrent files. 

While in the app, add torrents by pasting (`ctrl+v` or `v`) a magnet link or path to a `.torrent` file. 
You can also add torrents or magnet links via another terminal command line while the TUI is running (make sure to set a download path first):
```bash
# Magnet links or torrent paths can be pasted when the TUI is running.
crtl+v "magnet:?xt=urn:btih:..."
crtl+v "/absolute/path/to/my.torrent"

# CLI - Run in another terminal
superseedr "magnet:?xt=urn:btih:..."
superseedr "/absolute/path/to/my.torrent"
superseedr stop-client
```

Configuration files are located in the user's Application Support folder:
`Press [m] in the tui to see log and config path`


## Running with Docker

This is the recommended way to run `superseedr`, as it's the most flexible and stable setup.

> [!NOTE]  
> The OpenVPN and Wireguard docker setups below ensure **all** `superseedr` network activity is routed through a fully encrypted tunnel.
> To maintain this level of security, SOCKS5 proxies are intentionally not supported, as they do not encrypt your traffic.

**Prerequisites:** Ensure you have **Git** and **Docker Compose** installed.

### 1. Setup

1.  **Clone this repository:**
    ```bash
    git clone [https://github.com/Jagalite/superseedr.git](https://github.com/Jagalite/superseedr.git)
    cd superseedr
    ```

2.  **(Optional) Create your environment file:**
    ```bash
    cp .env.example .env
    ```

3.  **(Optional) Edit your `.env` file:**
    Open the `.env` file and uncomment the `HOST_...` paths if you want to store your config and downloads in local folders. If you leave them commented, Docker will safely manage the data in its own volumes.

### 2. Run Your Chosen Setup

> **Note:** You must use `docker compose run --rm superseedr` (not `up`) to correctly attach to the interactive Terminal UI.

#### Option 1: Standalone (Default)

This is the simplest setup and runs the client directly - no OpenVPN or WireGuard.
```bash
docker compose run --rm superseedr
```
#### Option 2: OpenVPN

This will route all of `superseedr`'s traffic through an OpenVPN tunnel, which acts as a kill-switch.

1.  In the `compose/openvpn/vpn-config/` directory, copy the example configs:
    ```bash
    cp compose/openvpn/vpn-config/superseedr.ovpn.example compose/openvpn/vpn-config/superseedr.ovpn
    cp compose/openvpn/vpn-config/auth.txt.example compose/openvpn/vpn-config/auth.txt
    ```
2.  Edit the new `superseedr.ovpn` and `auth.txt` with your credentials from your VPN provider.

3.  Run the OpenVPN stack:
    ```bash
    docker compose -f compose/openvpn/docker-compose.yml run --rm superseedr
    ```
#### Option 3: WireGuard

This will route all of `superseedr`'s traffic through a WireGuard tunnel.

1.  In the `compose/wireguard/wireguard-config/` directory, copy the example config:
    ```bash
    cp compose/wireguard/wireguard-config/wg0.conf.example compose/wireguard/wireguard-config/wg0.conf
    ```
2.  Edit the new `wg0.conf` with your settings from your VPN provider.

3.  Run the WireGuard stack:
    ```bash
    docker compose -f compose/wireguard/docker-compose.yml run --rm superseedr
    ```

### Installing from source
You can also install from source using `cargo`.
```bash
# Standard Build
cargo install superseedr

# Private Tracker Build
cargo install superseedr --no-default-features
```


## Current Status & Features

The client is in a late-alpha stage, with most core BitTorrent features implemented and functional.
Testing and refining for V1.0 release.

### Core Protocol & Peer Discovery
- **Real Time Performance Tuning:** Periodic resource optimizations (file handles) to maximize speeds and disk stability.
- **Peer Discovery:** Full support for Trackers, DHT, PEX, and Magnet Links (including metadata download).
- **Piece Selection:** Utilizes a Rarest-First strategy for optimal swarm health, switching to Endgame Mode for the final pieces.
- **Choking Algorithm:** Employs a tit-for-tat based choking algorithm with optimistic unchoking for efficient upload slot management.

### User Interface (TUI)
- **Real-time Dashboard:** A `ratatui`-based terminal UI displaying overall status, individual torrent progress, peer lists, and network graphs.
- **High Performance TUI:** FPS selector that allows 1-60fps.
- **Network Graph:** Historic time periods selector on network activity for network speed and disk failures.

### Configuration & Management
- **Persistent State:** Saves the torrent list, progress, and lifetime stats to a configuration file.
- **Speed Limits:** Allows setting global upload and download speed limits.

## Roadmap to V1.0
- **Testing:** Ongoing testing across various platforms and terminals.
- **Unit Testing:** Expansion of unit test coverage.
- **Bugs Startup and Shutdown** Fixing of serveral edge cases when users quit during certain critical phases.

## Roadmap to V1.5
- Fix and refactor synchronous startup and validation

## Future (V2.0 and Beyond)

### Refactors 
- **Codebase:** Reduce dependencies by implementing some of these features in the codebase.

### Networking & Protocol
- **Full IPv6 Support:** Allow connecting to IPv6 peers and announcing to IPv6 trackers, including parsing compact peers6 responses.
- **UPnP / NAT-PMP:** Automatically configure port forwarding on compatible routers to improve connectability.
- **Tracker Scraping:** Implement the ability to query trackers for seeder/leecher counts without doing a full announce (useful for displaying stats).
- **Network History:** Persisting network history to disk.

### Torrent & File Management
- **Selective File Downloading:** Allow users to choose which specific files inside a multi-file torrent they want to download.
- **Sequential Downloading:** Download pieces in order, primarily useful for streaming media files while they're downloading.
- **Torrent Prioritization / Queueing:** Allow users to set priorities for torrents and configure limits on the number of active downloading or seeding torrents.
- **Per-Torrent Settings:** Allow setting individual speed limits, ratio goals, or connection limits for specific torrents.
- **Torrent Log book:** Historic log book of all torrents added and deleted. Allows users to search and redownload.

### User Interface & Experience
- **Layout Edit Mode:** Allow the user to resize or drag and drop the layout of the panels.
- **RSS Feed Support:** Automatically monitor RSS feeds and download new torrents matching user-defined filters.
- **Advanced TUI Controls:** Add more interactive features to the TUI, like in-app configuration editing, more detailed peer/file views, advanced sorting/filtering.
- **TUI Files View Hierarchy:** Add a popup that shows in an interactive hierarchy view and live progress for the files of the torrent.
