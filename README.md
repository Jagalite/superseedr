<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Jagalite/superseedr-assets/main/superseedr_logo_transparent.gif">
  <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/Jagalite/superseedr-assets/main/superseedr_logo_light_transparent.gif">
  <img alt="Superseedr Logo" src="https://raw.githubusercontent.com/Jagalite/superseedr-assets/main/superseedr_logo_light_transparent.gif">
</picture>

# A BitTorrent Client in your Terminal

[![Rust](https://github.com/Jagalite/superseedr/actions/workflows/rust.yml/badge.svg)](https://github.com/Jagalite/superseedr/actions/workflows/rust.yml) [![Nightly Fuzzing](https://github.com/Jagalite/superseedr/actions/workflows/nightly.yml/badge.svg)](https://github.com/Jagalite/superseedr/actions/workflows/nightly.yml) ![GitHub release](https://img.shields.io/github/v/release/Jagalite/superseedr) [![crates.io](https://img.shields.io/crates/d/superseedr)](https://crates.io/crates/superseedr) [![Built With Ratatui](https://ratatui.rs/built-with-ratatui/badge.svg)](https://ratatui.rs/) <a title="This tool is Tool of The Week on Terminal Trove, The $HOME of all things in the terminal" href="https://terminaltrove.com/"><img src="https://cdn.terminaltrove.com/media/badges/tool_of_the_week/png/terminal_trove_tool_of_the_week_gold_transparent.png" alt="Terminal Trove Tool of The Week" /></a>

Superseedr is a modern Rust BitTorrent client featuring a high-performance terminal UI, real-time swarm observability, secure VPN-aware Docker setups, and zero manual network configuration. It is fast, privacy-oriented, and built for both desktop users and homelab/server workflows.

![Feature Demo](https://raw.githubusercontent.com/Jagalite/superseedr-assets/main/superseedr_landing.webp)

## 🚀 Features at a Glance

| **Experience** | **Networking** | **Engineering** |
| :--- | :--- | :--- |
| 🎨 **60 FPS TUI + Themes**<br>Fluid, animated interface with heatmaps and 40 live-switchable built-in themes. | 🐳 **Docker + VPN**<br>Gluetun integration with dynamic port reloading. | 🧬 **BitTorrent v2**<br>Hybrid swarms & Merkle tree verification. |
| 📰 **RSS Feeds**<br>In-app feed tracking, filtering, and ingest. | 🧩 **Cluster Mode**<br>OS-agnostic shared torrent catalog with automatic failover. | 🧠 **Self-Tuning**<br>Adaptive limits control for max speed and I/O Stability. |
| 🧲 **Magnet Links**<br>Native OS-level handler support. | 👻 **Private Mode**<br>Optional builds disabling DHT/PEX. | 📡 **Integrity Prober**<br>Continuous lightweight background integrity checks with fast recovery reprobes. |

### Terminal Torrenting With Superseedr

* **Pushing TUI Boundaries:** Experience a fluid, 60 FPS interface that feels like a native GUI, featuring smooth animations, high-density visualizations, and 40 built-in themes rarely seen in terminal apps.
* **See What's Happening:** Diagnose slow downloads instantly with deep swarm analytics, heatmaps, and live bandwidth graphs.
* **Set It and Forget It:** Automatic port forwarding and dynamic listener reloading in Docker ensure your connection stays alive, even if your VPN resets.
* **Crash-Proof Design:** Leverages Rust's memory safety guarantees to run indefinitely on low-resource servers without leaks or instability, and shared cluster mode adds automatic failover across hosts.

<p align="center">
  <img src="https://raw.githubusercontent.com/Jagalite/superseedr-assets/main/superseedr-matix.gif"/>
</p>

## Installation

Download platform-specific installers from the [releases page](https://github.com/Jagalite/superseedr/releases) **(includes browser magnet link support)**:
- Windows: `.exe` per-user installer (no admin) or `.msi` installer
- macOS: `.pkg` installer  
- Debian/Ubuntu: `.deb` package

### Package Managers
- **Cargo:** `cargo install superseedr`
- **Brew:** `brew install superseedr`
- **Arch Linux:** `yay -S superseedr` (via AUR)

[![Packaging status](https://repology.org/badge/vertical-allrepos/superseedr.svg)](https://repology.org/project/superseedr/versions)

## Usage
Open a terminal
```bash
superseedr
```
### ⌨️ Key Controls
| Key | Action |
| :--- | :--- |
| `m` | **Open full manual / help** |
| `Q` | Quit |
| `↑` `↓` `←` `→` | Navigate |
| `c` | Configure Settings |

> [!TIP]  
> Add torrents by clicking magnet links in your browser or opening .torrent files.
> Copying and pasting (ctrl + v) magnet links or paths to torrent files will also work.

## Troubleshooting

**Connection or Disk issues?**
- Check your firewall allows outbound connections
- Increase file descriptor limit: `ulimit -n 65536`
- For VPN users: Verify Gluetun is running and connected

**Slow downloads?**
- Enable port forwarding in your VPN settings
- Check the swarm health in the TUI's analytics view

**More help:** See the [FAQ](docs/FAQ.md) or [open an issue](https://github.com/Jagalite/superseedr/issues)

## More Info
- 🤝[Contributing](CONTRIBUTING.md): How you can contribute to the project (technical and non-technical).
- ❓[FAQ](docs/FAQ.md): Find answers to common questions about Superseedr.
- 🔒[Native Network Binding](docs/native-network-binding.md): Bind owned traffic and DNS to a selected interface with fail-closed recovery.
- 📜[Changelog](docs/CHANGELOG.md): See what's new in recent versions of Superseedr.
- 🗺️[Roadmap](docs/ROADMAP.md): Discover upcoming features and future plans for Superseedr.
- 🧑‍🤝‍🧑[Code of Conduct](CODE_OF_CONDUCT.md): Understand the community standards and expectations.

## 🐳 Running with Docker

Superseedr offers a fully secured Docker setup using Gluetun. All BitTorrent traffic is routed through a VPN tunnel with dynamic port forwarding and zero manual network configuration.

If you want privacy and simplicity, Docker is the recommended way to run Superseedr.

Follow steps below to create .env and .gluetun.env files to configure OpenVPN or WireGuard.

```bash
# Docker (No VPN):
# Uses internal container storage. Data persists until the container is removed.
docker run -it jagatranvo/superseedr:latest

# Docker Compose (Gluetun with your VPN):
# Requires .env and .gluetun.env configuration (see below).
docker compose up -d && docker compose attach superseedr
```

<details>
<summary><strong>Click to expand Docker Setup</strong></summary>

### Setup

1.  **Get the Docker configuration files:**
    You only need the Docker-related files to run the pre-built image, not the full source code.

    **Option A: Clone the repository (Simple)**
    This gets you everything, including the source code.
    ```bash
    git clone https://github.com/Jagalite/superseedr.git
    cd superseedr
    ```
    
    **Option B: Download only the necessary files (Minimal)**
    This is ideal if you just want to run the Docker image.
    ```bash
    mkdir superseedr
    cd superseedr

    # Download the compose file and example config files
    curl -sL \
      -O https://raw.githubusercontent.com/Jagalite/superseedr/main/docker-compose.yml \
      -O https://raw.githubusercontent.com/Jagalite/superseedr/main/.env.example \
      -O https://raw.githubusercontent.com/Jagalite/superseedr/main/.gluetun.env.example

    # Note the example files might be hidden run the commands below to make a copy.
    cp .env.example .env
    cp .gluetun.env.example .gluetun.env
    ```

2.  **Recommended: Create your environment files:**
    * **App Paths & Build Choice:** Edit your `.env` file from the example. This file controls your data paths and which build to use.
        ```bash
        cp .env.example .env
        ```
        Edit `.env` to set your absolute host paths (e.g., `HOST_SUPERSEEDR_ROOT_PATH=/my/path/seedbox`). **This is important:** it maps the container's shared seedbox root (`/seedbox`) to a real folder on your computer. Keep `superseedr-config/` inside that root for the simplest shared-config setup.

    * **VPN Config:** Edit your `.gluetun.env` file from the example.
        ```bash
        cp .gluetun.env.example .gluetun.env
        ```
        Edit `.gluetun.env` with your VPN provider, credentials, and server region.

#### Option 1: VPN with Gluetun (Recommended)

Gluetun provides:
- A VPN kill-switch
- Automatic port forwarding
- Dynamic port changes from your VPN provider

Many VPN providers frequently assign new inbound ports. Most BitTorrent clients must be restarted when this port changes, breaking connectability and slowing downloads.
Superseedr can detect Gluetun’s updated port and reload the listener **live**, without a restart, preserving swarm performance.

1.  Make sure you have created and configured your `.gluetun.env` file.
2.  Run the stack using the default `docker-compose.yml` file:

```bash
docker compose up -d && docker compose attach superseedr
```
> To detach from the TUI without stopping the container, use the Docker key sequence: `Ctrl+P` followed by `Ctrl+Q`.
> **Optional:** press `[z]` first to enter power-saving mode.

---

#### Option 2: Direct docker run

This runs the client directly without Gluetun. It is useful for advanced users who want to manage networking themselves.

    docker run --rm -it \
      -e SUPERSEEDR_DEFAULT_DOWNLOAD_FOLDER=/seedbox \
      -e SUPERSEEDR_SHARED_CONFIG_DIR=/seedbox \
      -e SUPERSEEDR_SHARED_HOST_ID=seedbox-docker \
      -p 6881:6881/tcp \
      -p 6881:6881/udp \
      -v /your/seedbox:/seedbox \
      -v ./docker-data/share:/root/.local/share/jagalite.superseedr \
      jagatranvo/superseedr:latest

Replace /your/seedbox with the shared seedbox root on your host.
Keep superseedr-config/ inside that folder so the container sees it at /seedbox/superseedr-config.

</details>

## 🔗 Integrations & Automation

Superseedr is built around a local CLI and a file-based automation model, so
you can script, queue, and inspect work without exposing a network control
stack. The same command flow works when a client is online, when it is offline,
and in shared mode when you are operating against a remote leader through a
mounted shared root.

Check out the [Superseedr Plugins Repository](https://github.com/Jagalite/superseedr-plugins) for plugins (beta testing).

<details>
<summary><strong>Click to expand automation details</strong></summary>

### 1. File Watcher & Auto-Ingest
Superseedr uses a file-based watch-folder architecture so local automation,
scripts, containers, and other processes can control ingestion without needing a
separate daemon protocol.

Each node can watch a local `watch_folder`. In standalone mode, that watch
folder feeds the local client directly. In shared mode, followers watch their
own local folders and relay supported files into the shared inbox so the leader
can process them and update the shared catalog.

Processed watch files are archived after handling so the queue stays
deterministic and auditable.

| File Type | Action |
| :--- | :--- |
| **`.torrent`** | Adds a torrent from a torrent file. In shared mode, follower-side ingest may stage the torrent for leader processing. |
| **`.magnet`** | Adds a torrent from a magnet link stored as text. |
| **`.path`** | Adds a torrent from a referenced torrent-file path. In shared mode, cross-host handling uses portable shared-root-aware staging. |
| **`.control`** | Applies queued control requests such as pause, resume, remove, purge, and priority changes. |
| **`shutdown.cmd`** | Requests graceful shutdown of the running client or shared leader. |

See [`docs/shared-config.md`](docs/shared-config.md) for shared inbox and
leader/follower watch-folder behavior.

### 2. CLI Control
The CLI uses the same file-oriented control model. Depending on mode, commands
either:

- write control files for a running client
- queue requests through the shared inbox for the leader
- or apply offline mutations directly when no runtime is available

That makes the CLI easy to script from shells, containers, task runners, and
other local automation.

See [`docs/cli.md`](docs/cli.md) for the full CLI guide.

```bash
# Add a magnet link
superseedr add "magnet:?xt=urn:btih:..."

# Add a torrent file by path
superseedr add "/path/to/linux.iso.torrent"

# Inspect the current shared launcher selection
superseedr show-shared-config

# Show resolved config, log, status, journal, and watch paths
superseedr show-configs

# Persist shared launcher config for installed/protocol launches
superseedr set-shared-config "/path/to/seedbox"

# Convert local config into layered shared config
superseedr to-shared "/path/to/seedbox"

# Convert the active shared config back into local standalone config
superseedr to-standalone

# Stop the client gracefully
superseedr stop-client
```

See [`docs/cli.md`](docs/cli.md) for full CLI command behavior, and
[`docs/shared-config.md`](docs/shared-config.md) for shared leader/follower
routing.

### 3. Status API & Monitoring
For external dashboards, health checks, and lightweight automation, Superseedr
periodically dumps runtime state to JSON.

* **Output Location:** a status JSON file in the runtime data area.
* **Shared Mode:** each host writes its own status file, and shared CLI status follows the current leader snapshot.
* **Content:** includes transfer stats, runtime metrics, and torrent-level state.

#### Configuration
You can control how often this file is updated using the `output_status_interval` setting.

**Environment Variable:**
Set this variable in your Docker config to change the update frequency (in seconds).
```bash
# Update the status file every 5 seconds
SUPERSEEDR_OUTPUT_STATUS_INTERVAL=5
```

### 4. RSS Feeds & History
Superseedr can track RSS feeds in-app, evaluate feed items against your configured
matching rules, and automatically ingest matching releases without needing an
external automation stack.

* **Feed Tracking:** monitor RSS feeds directly from the client.
* **Rule-Based Matching:** use configured match rules to decide what should be ingested.
* **Auto-Ingest:** matching items can be queued into the normal torrent ingest path.
* **History & Deduplication:** downloaded feed history is persisted so the same item is not re-ingested repeatedly.

RSS download history is capped at **1000 entries**.

* When the history grows past 1000, the **oldest entries are pruned** first.
* This limit applies to persisted runtime history in `persistence/rss.toml`.

</details>

## 🧩 Shared Configurations & Cluster Mode

Shared mode gives you an OS- and machine-agnostic torrent catalog and settings
that live alongside your data on the NAS or shared root. Any Superseedr client
that mounts that shared root can connect and reuse the same catalog in real time.
Superseedr CLI commands work against that shared config both online and offline. See
[`docs/shared-config.md`](docs/shared-config.md) for the full shared-mode guide.

```text
Same shared root, different local mount paths

NAS
/shared/superseedr
├─ superseedr-config/
│  ├─ settings.toml
│  ├─ catalog.toml
│  └─ ...
└─ video1.mkv

macOS
$ superseedr set-shared-config /Volumes/superseedr-mount
$ superseedr
/Volumes/superseedr-mount
├─ superseedr-config/
│  ├─ settings.toml
│  ├─ catalog.toml
│  └─ ...
└─ video1.mkv

Windows
> superseedr set-shared-config "X:\superseedr-mount"
> superseedr
X:\superseedr-mount
├─ superseedr-config\
│  ├─ settings.toml
│  ├─ catalog.toml
│  └─ ...
└─ video1.mkv
```

Cluster mode turns that shared catalog into an active multi-node setup. One node
acts as leader and updates shared desired state, while other nodes stay online
as followers that continue seeding and apply the leader-written catalog in real
time. If the leader goes away, another node can take over automatically, and
each host can mount the same shared root at a different local path for cross-OS
operation.

```text
                    Shared Root / NAS
                      /shared/superseedr
                  ┌───────────────────────┐
                  │ superseedr-config/    │
                  │ settings.toml         │
                  │ catalog.toml          │
                  │ inbox/                │
                  │ hosts/                │
                  └───────────────────────┘
                          ↑        ↑
                          │        │
                       Leader   Follower

       ┌──────────────────────┐    ┌──────────────────────┐
       │ Windows              │    │ macOS                │
       │ X:\superseedr-mount  │    │ /Volumes/superseedr- │
       │                      │    │ mount                │
       └──────────────────────┘    └──────────────────────┘
```


## 🧠 Advanced: Architecture & Engineering

Superseedr is built on a **Reactive Actor** architecture verified by model-based fuzzing, ensuring stability under chaos. It features a **Self-Tuning Resource Allocator** that adapts to your hardware in real-time and a hybrid **BitTorrent v2** engine, all powered by asynchronous **Tokio** streams for maximum throughput.

<details>
<summary><strong>Click to expand technical internals</strong></summary>

This section is designed for developers, contributors, and AI agents seeking to understand the internal design decisions that drive Superseedr's performance.

### ⚡ Async Networking Core
Superseedr is built on the **Tokio** runtime, leveraging asynchronous I/O for maximum concurrency.
* **Full-Duplex Streams:** Every peer connection is split into independent **Reader** and **Writer** tasks (`tokio::io::split`). This allows the client to saturate download and upload bandwidth simultaneously without thread blocking or lock contention, ensuring the UI remains responsive even with thousands of active connections.
* **Actor-Based Session Management:** Each peer operates as an isolated Actor. Communication between the network socket and the core logic happens exclusively via `mpsc` channels, meaning a slow or misbehaving peer cannot block the main event loop or affect other connections.
* **Hot-Swappable Listeners:** The application runs an async file watcher (`notify`) on the VPN configuration volume. When **Gluetun** rotates the forwarded port, Superseedr detects the file change and instantly rebinds the TCP listener to the new port without dropping the swarm state or restarting the process.

### DHT Runtime & Demand Planner
Superseedr ships a first-party Mainline DHT implementation instead of treating DHT as a black-box peer source.
* **Dual-Stack Runtime:** The internal runtime maintains IPv4 and IPv6 UDP transports, routing tables, peer storage, bootstrap state, and rotating announce tokens while serving inbound `find_node`, `get_peers`, and `announce_peer` traffic.
* **Client-Aware Demand:** Torrent managers feed demand state and live swarm metrics into the DHT service. The planner prioritizes metadata recovery and peer-starved torrents first, then spends additional query budget on active swarms that are still producing useful peers.
* **Pause/Resumable Crawls:** Lookup slices can be parked when their wall-time budget expires, preserving traversal state instead of throwing away the crawl frontier. Later planner slices can resume the crawl from the saved state, while the drain path still captures late peers from in-flight queries.
* **Adaptive Query Pressure:** DHT work is bounded by lookup slots, per-class budgets, late-peer drain handling, and peer-slot pressure. When the client is full, DHT power can ramp down quickly; when capacity returns, it ramps back up gradually.
* **Protocol Hardening:** The runtime validates response sources, filters unroutable nodes, tracks suspicious identity churn, rate-limits inbound KRPC traffic, and keeps DHT participation disabled entirely in private builds.
* **Deterministic Verification:** Planner and runtime reducers are covered by deterministic replay tests, invariant checks, and property tests for lookup traversal, scheduling, demand selection, drain behavior, and peer-pressure scaling.

### 🔒 Security & Privacy Engineering
* **VPN Isolation (Kill-Switch):** In the Docker Compose setup, Superseedr's network stack is fully routed through **Gluetun**. This guarantees that 100% of BitTorrent traffic traverses the VPN tunnel. If the tunnel drops, connectivity is cut immediately, preventing any IP leakage over the host connection.
* **Binary-Level Private Mode:** Private tracker compliance is enforced at compile time, not just runtime. By building with `--no-default-features`, the DHT and Peer Exchange (PEX) modules are completely excluded from the binary, guaranteeing zero leakage of private swarms.

### 🏗️ Reactive Actor Model & Verification
The application logic abandons traditional mutex-heavy threading in favor of a **Functional Reactive** architecture.
* **Deterministic State Machine:** The `TorrentManager` operates as a Finite State Machine (FSM). External events (Network I/O, Timer Ticks) are transmuted into `Action` enums, processed purely in memory, and result in a list of `Effects`.
* **Chaos Engineering:** We validate this core logic using **Model-Based Fuzzing** (via Proptest). Our test suite injects deterministic faults to verify correctness under hostile conditions:
* **Network Chaos:** Simulates **Packet Loss** (dropped actions), **High Latency** (reordered actions), and **Duplication** (ghost packets).
* **Malicious Peers:** Fuzzers act as "Bad Actors" that send protocol violations, infinite byte-streams, and out-of-bounds requests to ensure the engine punishes them without crashing.

### 🤖 Self-Tuning Resource Allocator
Instead of static `ulimit` values, Superseedr runs a **Stochastic Hill Climbing** optimizer in the background.
* **The Loop:** Every 90 seconds, it randomly reallocates internal permits between competing resources—**Peer Sockets**, **Disk Read Slots**, and **Disk Write Slots**—to find the local maximum for performance.
* **Universal Optimization:** This algorithm dynamically discovers the optimal configuration for *any* combination of hardware (SSD vs HDD) and network environment (Home Fiber vs Datacenter), automatically scaling concurrency to match capacity.

### 📡 Integrity Prober
Superseedr automatically and continuously checks completed torrents in the background without falling back to blunt full-library rescans.
* **Designed for Scale:** Integrity work is split into small bounded batches, keeping checks cheap even across very large collections.
* **Fast Fault Detection:** Foreground disk-read failures immediately trigger targeted recovery reprobes, surfacing missing or damaged data quickly.
* **No-Config Recovery:** Healthy torrents are monitored automatically, while unavailable torrents are prioritized for fast recovery detection without extra setup.

### 🧮 Statistical Engine
Superseedr calculates granular metrics in real-time to drive optimization and observability:
* **IOPS & Latency:** Tracks instantaneous Input/Output Operations Per Second and uses an Exponential Moving Average (EMA) to calculate precise Read/Write latency (ms). This helps distinguish between bandwidth limits and disk saturation.
* **Disk Thrash Score:** Measures physical disk head movement using `Sum(|Offset - PrevOffset|) / Ops`. This detects random I/O bottlenecks that raw speed metrics miss.
* **Seek Cost per Byte (SCPB):** Calculates the "expense" of I/O relative to throughput (`TotalSeekDistance / TotalBytes`), serving as the primary penalty factor for the self-tuner.

### ♟️ Protocol Algorithms
Superseedr implements optimized versions of the core BitTorrent exchange strategies:
* **Selective & Priority Downloading:** Support for file-level priority (Skip, Normal, High). The engine maps file boundaries to pieces, prioritizing high-value data while ensuring shared boundary pieces are handled correctly to prevent corruption.
* **Rarest-First Piece Selection:** The client continuously tracks piece availability across the swarm, prioritizing rare pieces to prevent "swarm starvation" and ensure redundant availability.
* **Tit-for-Tat Choking:** The choking algorithm uses a robust Tit-for-Tat strategy (reciprocation), rewarding peers who provide the highest bandwidth while optimistically unchoking new peers to discover better connections.

### 🔬 Unique Visualizations & UX
Superseedr includes specialized TUI components (`src/tui/view.rs`) to visualize data usually hidden by other clients:
* **Integrated File Explorer:** A custom, navigable filesystem browser that provides instant previewing of `.torrent` file contents and internal directory structures before the download begins.
* **Block Particle Stream:** A vertical "Matrix-style" flow visualizing individual 16KB data blocks entering (Blue) or leaving (Green).
* **Peer Lifecycle Scatterplot:** Tracks the exact moment peers are Discovered, Connected, and Disconnected to visually diagnose swarm "churn."
* **Backpressure Markers:** The network graph overlays red "Backpressure Events" whenever the self-tuner detects a system limit (e.g., file descriptors), proving the engine is actively managing load.

### 🧬 Hybrid BitTorrent v2 (BEP 52)
Superseedr implements the full **Merkle Tree** verification stack required for BitTorrent v2.
* **Block-Level Validation:** Incoming data is hashed and verified at the 16KiB block level using Merkle Proofs, allowing for the immediate rejection of corrupt data before it is written to disk.
* **Hybrid Swarms:** The client handles `VerifyPieceV2` effects to simultaneously handshake with legacy v1 peers (SHA-1) and modern v2 peers (SHA-256).

### 🛡️ Backpressure & Flow Control
* **Persistent Retries with Backoff:** Critical I/O operations (like disk writes) are protected by an exponential backoff retry mechanism (jittered), ensuring transient system locks or busy disks don't crash the download session.
* **Adaptive Pipelining:** The `PeerSession` uses a dynamic sliding window (AIMD-like algorithm) that expands or shrinks the request queue based on the peer's real-time response rate (`blocks_received_interval`), maximizing link saturation.
* **Token Buckets:** Global bandwidth is shaped via a hierarchical Token Bucket algorithm that enforces rate limits without blocking async executors.

### 📜 Key Standards Compliance
Superseedr implements the following BitTorrent Enhancement Proposals (BEPs):
* **BEP 3:** The BitTorrent Protocol Specification
* **BEP 5:** DHT Protocol (Mainline)
* **BEP 9:** Extension for Peers to Send Metadata Files (Magnet Links)
* **BEP 10:** Extension Protocol
* **BEP 11:** Peer Exchange (PEX)
* **BEP 19:** WebSeed - HTTP/FTP Seeding
* **BEP 52:** The BitTorrent Protocol v2

</details>



