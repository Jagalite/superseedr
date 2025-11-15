# superseedr - A BitTorrent Client in your Terminal

A **standalone** BitTorrent client created with **[Ratatui](https://ratatui.rs/)**.

It features a custom-built BitTorrent protocol implementation in Rust focused on observability. Includes private builds and vpn-enabled docker setups.

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
2.  **Create your environment files:**
    * **App Paths & Build Choice:** Create your `.env` file from the example. This file controls your data paths and which build to use.
        ```bash
        cp .env.example .env
        ```
        Edit `.env` to set your absolute host paths (e.g., `HOST_SUPERSEEDR_DATA_PATH=/my/path/data`).

        **To use the Private Build**, edit `.env` and change the `IMAGE_NAME` to point to the `:private` tag:
        ```ini
        # .env file
        IMAGE_NAME=jagatranvo/superseedr:private
        ```
        If you leave this commented out, it will default to the public `:latest` build.

    * **VPN Config:** Create your `.gluetun.env` file from the example.
        ```bash
        cp .gluetun.env.example .gluetun.env
        ```
        Edit `.gluetun.env` with your VPN provider, credentials, and server region.

#### Option 1: VPN with Gluetun (Recommended)

This setup routes all `superseedr` traffic through a secure Gluetun VPN tunnel, which acts as a kill-switch and handles dynamic port forwarding from your provider.

1.  Make sure you have created and configured your `.gluetun.env` file.
2.  Run the stack using the default `docker-compose.yml` file:

* **Interactive:**
    ```bash
    docker compose run --rm superseedr superseedr
    ```
* **Detached:**
    ```bash
    docker compose up -d
    docker compose exec superseedr superseedr
    ```

---

#### Option 2: Standalone

This runs the client directly, exposing its port to your host. It's simpler but provides no VPN protection.

1.  Run using the `docker-compose.standalone.yml` file:

* **Interactive:**
    ```bash
    docker compose -f docker-compose.standalone.yml run --rm superseedr
    ```
* **Detached:**
    ```bash
    docker compose -f docker-compose.standalone.yml up -d
    docker compose -f docker-compose.standalone.yml exec superseedr superseedr
    ```

---
### (Advanced) Building from Source

If you want to test your own local code changes, you can build the image yourself.

1.  Build your local image:
    ```bash
    docker build -t superseedr .
    ```
2.  If you haven't already, create your `.env` file (`cp .env.example .env`).
3.  Edit your `.env` file and set `IMAGE_NAME=superseedr`.

Now, when you run any of the commands above, Docker Compose will use your local `superseedr` image instead of pulling the pre-built one.


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
