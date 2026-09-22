# Superseedr Architecture

## Overview
Superseedr is a high-performance, asynchronous BitTorrent client featuring a terminal user interface (TUI). It is built using **Rust** and **Tokio**, employing an Actor-like concurrency model to manage high-throughput networking and disk I/O without blocking the UI thread.

## High-Level Design
The application is divided into three distinct layers:
1.  **Presentation Layer (`App` & `TUI`):** Handles user input, rendering, and state aggregation.
2.  **Orchestration Layer (`TorrentManager`):** Manages the logic for individual torrents (peer selection, piece picking, state mutation).
3.  **Network/IO Layer (`PeerSession` & Storage):** Handles raw TCP communication, BitTorrent protocol parsing, and disk operations.


## Core Components

### 1. The Application Loop (`src/app/mod.rs` & `src/native/entrypoint.rs`)
The `App` struct is the central owner of the application state. It does not handle heavy lifting (downloading/hashing); instead, it acts as a controller and visualizer.

`src/native/entrypoint.rs` owns native process startup, including CLI parsing, logging setup, panic hooks, and launching the application loop. `src/main.rs` is intentionally a thin Tokio wrapper that delegates to the library's `run_native` entrypoint.

* **Responsibility:**
    * Initializes the `ResourceManager` and `TcpListener`.
    * Spawns `TorrentManager` tasks for each torrent.
    * Aggregates metrics via a `broadcast` channel (`metrics_tx`/`rx`) to update the UI.
    * Handles User Input (keyboard events) and mutates `AppState`.
* **State Management:**
    * `AppState`: Contains display-optimized data (sparkline history, peer lists, throughput graphs).
    * `Settings`: Configurable constraints (limits, paths).

### 2. The TUI (`src/tui.rs`)
The UI is built using `Ratatui`. It is stateless regarding logic; it simply renders the current snapshot of `AppState`.

* **Key Features:**
    * **Sparklines:** Visualizes download/upload history.
    * **Heatmaps:** Renders swarm availability.
    * **Throttling:** Redraws are capped (e.g., 30 FPS or 60 FPS) to save CPU.

### 3. Torrent Manager (`src/torrent_manager/manager.rs`)
This is the "Brain" of a specific torrent. Each torrent runs as an isolated Tokio task (Actor).

* **Architecture Pattern: Action/Effect:**
    Instead of mutating state ad-hoc, the manager uses a functional reactive pattern.
    1.  **Event:** An event arrives (e.g., `IncomingBlock`, `PeerConnected`).
    2.  **Action:** The event is converted into an `Action` enum.
    3.  **Update:** The `TorrentState` processes the `Action` and returns a list of **Effects** (e.g., `WriteToDisk`, `SendToPeer`).
    4.  **Side Effect:** The manager executes the effects asynchronously.

* **Responsibility:**
    * Manages the `PieceManager` (Bitfield logic).
    * Tracks all connected peers (`PeerState`).
    * Handles file validation and hash checking.
    * Interfaces with the DHT and Trackers.


### 4. Peer Session (`src/networking/session.rs`)
Represents a single TCP connection to a peer. It implements the BitTorrent Wire Protocol.

* **Concurrency:**
    * Splits the TCP stream into a **Reader Task** and a **Writer Task**.
    * **Reader Task:** Parsers raw bytes into `Message` enums and sends them to the session loop.
    * **Writer Task:** Batches outgoing messages and flushes them to the TCP socket to reduce syscall overhead.
* **Congestion Control:**
    * Implements a dynamic sliding window ("Adaptive Pipelining") to optimize request throughput based on peer latency.
    * Uses a `Semaphore` to limit "blocks in flight".

### 5. Resource Management
* **`ResourceManager` (`src/resource/native.rs`):** A centralized gatekeeper that limits the number of open file descriptors and active sockets (semaphores) to prevent OS resource exhaustion (e.g., `ulimit`).
* **`TokenBucket` (`src/token_bucket.rs`):** Implements global bandwidth rate limiting for downloads and uploads.

## Data Flow: Downloading a Block

The flow of data from the network to the disk demonstrates the interaction between layers:

1.  **Network**: `PeerSession` Reader Task receives bytes from TCP.
2.  **Protocol**: Bytes are parsed into a `Message::Piece`.
3.  **Validation**: `PeerSession` verifies the block was actually requested.
4.  **Command**: `PeerSession` sends `TorrentCommand::Block` to `TorrentManager`.
5.  **State Update**: `TorrentManager` calculates the `Action::IncomingBlock`.
6.  **Effect**: The State generates an `Effect::WriteToDisk`.
7.  **IO**: `TorrentManager` acquires a write permit from `ResourceManager` and spawns a blocking task to write data to storage.
8.  **Completion**: Upon success, an event is sent back to mark the piece as received.

## Concurrency Model

Superseedr relies heavily on Tokio's message-passing primitives:

* **mpsc (Multi-Producer, Single-Consumer):**
    * `PeerSession` -> `TorrentManager` (Reporting data/events).
    * `TorrentManager` -> `PeerSession` (Sending requests/chokes).
    * `App` -> `TorrentManager` (User commands like Pause/Delete).
* **broadcast (Multi-Producer, Multi-Consumer):**
    * `TorrentManager` -> `App` (Broadcasting metrics for the UI).
    * `App` -> All Components (Global Shutdown signal).
* **oneshot:**
    * Used for handling internal errors between Reader/Writer tasks.

## Code Map
| File | Responsibility |
| :--- | :--- |
| `src/main.rs` | Thin Tokio binary wrapper that calls `superseedr::run_native`. |
| `src/native/entrypoint.rs` | Native CLI parsing, logging setup, panic hooks, and application startup. |
| `src/app/mod.rs` | Global state container, input handling, metrics aggregation. |
| `src/tui.rs` | Rendering logic using Ratatui widgets. |
| `src/torrent_manager/manager.rs` | The Actor managing a specific torrent's lifecycle. |
| `src/networking/session.rs` | TCP connection handling and BitTorrent protocol parsing. |
| `src/networking/protocol.rs` | Binary serialization/deserialization of Wire Protocol messages. |
