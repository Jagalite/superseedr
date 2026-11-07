# Headless Mode and `superseedr attach`

Status: phases 1–3 implemented (read-only attach, Unix only). Phase 4 not started.

## Goal

Run the engine without a terminal (`superseedr --headless`) and let any number
of TUI clients connect to it (`superseedr attach`), detach, and reconnect
without disturbing downloads.

Keep it simple. Phase 1 clients are read-only.

## Non-Goals (for now)

- Writes from attached clients (pause, add, config edits, ...).
- Remote/TCP access or authentication beyond local socket permissions.
- Windows transport (named pipes).
- Delta encoding, compression, or bandwidth tuning before measurement says so.
- Changing how normal (non-headless) mode works.

## Model

The daemon owns the engine. Clients own their view.

```
superseedr --headless                        superseedr attach (N clients)
  engine + AppState                            normal TUI, own AppState copy
  builds EngineSnapshot each tick  ──data──▶   replaces local engine data,
                                               renders, handles keys locally
            └──── host-local Unix socket (0600) ────┘
```

- Only data crosses the socket. No keystrokes, no rendered frames.
- Each client renders in its own process, so selection, screen, sort, and theme
  are naturally per-client. No shared view state on the daemon.
- This mirrors shared-config cluster mode: the daemon acts like a leader, an
  attached client acts like a follower that has no engine of its own.

Normal mode is unchanged: one process, the local terminal, no socket, no
serialization.

## Usage

```
superseedr --headless     # engine only; prints the socket path, logs to files
superseedr attach         # read-only TUI; Q detaches, the engine keeps running
superseedr stop-client    # stops the headless engine (SIGTERM works too)
```

The socket is `superseedr.sock` in the host-local runtime data directory (never
the shared root: Unix sockets do not work on network filesystems). Set
`SUPERSEEDR_ATTACH_SOCKET` for both commands to use another path, e.g. when the
default exceeds the ~100-byte socket path limit. Point it into a directory only you
can write (not a shared `/tmp`). A second `--headless` on the
same host is refused while one is serving (a `<socket>.lock` file held for the
daemon's lifetime serializes startup); a leftover socket file from a crash is
replaced, but never a file that is not a socket. Connections from other users
are refused.

## Wire Protocol (phase 1)

One-way: daemon → client. A Unix socket rather than a FIFO, so each client gets
its own stream.

```
Header   { protocol, daemon_version }
Snapshot { ... }        # full, on connect
Snapshot { ... }        # full, every stats tick (1s)
...
```

- Frames are a big-endian `u32` length plus JSON (binary later, if measurement
  says so).
- The daemon only captures snapshots while a client is attached, and serializes
  off the event loop.
- The client checks `protocol` and disconnects with a clear message on mismatch.
- The daemon serializes once per tick and shares the bytes with every client.
- Each client has a latest-wins slot: a slow client skips stale snapshots, and
  the engine never waits on a socket.
- A write error (client gone) drops that client.

Full snapshots keep every message self-contained. If measurement on a large
catalog shows they are too big, make only the heavy sections incremental
(append-only histories, changed torrents) with periodic full snapshots. Keep the
snapshot split into sections so that change stays local.

## Measurements (phase 3)

Ignored test `app::snapshot::tests::measure_snapshot` (optimized build, LTO off).
Synthetic catalog sized like seed1: 126 torrents × 2,048 pieces × 30 connected
peers each, per-torrent histories at their real 200-point cap, plus seed1's real
persisted network/activity history and event journal.

| Section | Bytes per snapshot |
|---|---|
| status | 2 KB |
| network + activity history | 0.9 MB |
| event journal | 0.95 MB |
| torrents (display) | 0.7 MB |
| peers (packed bitfields) | 3.5 MB |
| **total** | **6.2 MB** (once per second, only while attached) |

- Daemon event loop: 2.5 ms per tick to capture (clone). Serializing (20 ms)
  runs on a blocking thread.
- Client: 20 ms to decode, under 1 ms to apply.
- Before packing, peer bitfields as JSON `true`/`false` arrays were 45 MB of a
  56 MB snapshot. They are now packed bits in hex (`WirePeer`).

Every torrent having 30 peers is a pessimistic case; idle seeds have none. No
incremental sections are needed at this size. If they become necessary, the
journal and histories are append-only and the peers section is the largest.

## Read-Only Client Rules

- Allowed locally: navigation, selection, search, help, sorting, chart and graph
  modes, theme (not persisted).
- Refused with a "read-only client" message: every engine-writing effect
  (control requests, config apply, RSS actions, pasted adds, file browser on the
  daemon's filesystem).
- `Q` (the normal quit key) detaches. `superseedr stop-client` stops the daemon.
- The refusal notice stays visible across snapshots until dismissed.

## Phases

1. **Read model.** Define a serializable `EngineSnapshot` covering everything
   `draw` reads from engine state, plus `Settings`, `DhtStatus`, and
   `DhtWaveTelemetry` (passed to `draw` separately today).
   `capture(&AppState, ...)` builds it on the daemon; `apply(&mut AppState)`
   writes it into a client's local `AppState`. Normal mode never builds one, so
   it is untouched.
2. **Headless + read-only attach.** `--headless` startup with no local terminal,
   socket listener, snapshot broadcast, and the `attach` client.
3. **Measure.** Snapshot size and serialization time per tick on a large catalog.
   Add incremental sections only if needed.
4. **Writes (later).** Client → daemon `ControlRequest` on the same socket, with
   per-request results. Add the missing request variants (settings patch with a
   base revision, theme, RSS, display rate, journal replay, previews). The inbox
   keeps working for CLI, watch-folder, and cluster users through the same
   handler.

## Read Model Inventory

What the TUI reads from `AppState`, excluding view state it owns locally
(`ui`, `mode`, `screen_area`, sorts, chart and graph modes, theme, pending
input):

- Torrents: `torrents`. `torrent_list_order` is the client's own sorted and
  filtered list, rebuilt locally after each snapshot.
- Rates and histories: `avg_download_history`, `avg_upload_history`,
  `network_history_state`, `activity_history_state`, `event_journal_state`,
  `avg_disk_*_bps`,
  `avg_disk_*_latency`, `*_iops`, `recv_to_write_p95`.
- Resources and tuning: `limits`, `last_tuning_limits`, `*_tuning_score`,
  `tuning_countdown`, `effective_download_limit_bps`, `active_peer_limit`,
  `adaptive_max_scpb`, `*_thrash_score`, `disk_health_*`, `cpu_usage`,
  `ram_usage_percent`, `app_ram_usage`.
- Session: `session_total_*`, `lifetime_*_from_config`, `run_time`,
  `startup_load_progress`, `startup_expected_seeds`, `shutdown_progress`.
- Status: `system_warning`, `system_error`, `update_available`,
  `network_runtime_status`, `network_activation_status`,
  `externally_accessable_port_v4/v6`, `cluster_*_label`, `runtime_paths`,
  `rss_runtime`, pending ingest/control/watch counts.

Also: `inbound_peer_transports`, `peer_policy`, `peer_manager_view` (client
recomputes its derived peer rows), `auto_graph_activity` (client runs the auto
graph window update), and queue lengths as counts.

Watch for fields the existing types skip for their own serialization:
`TorrentMetrics` skips peers, block histories, and reconnect counts (it is also
the status-file format), and `Settings` skips `randomize_client_port`. The
snapshot carries those explicitly (`MetricsExtras`). `Instant` fields only drive
short animations and are skipped.

## Known Gaps (phase 2)

- While any client is attached, the daemon keeps its peer manager view active
  (normally only while its own peer screen is open), because the one-way stream
  cannot say which screen a client shows.
- Skipped `Instant` fields: recent file activity flashes, startup pacer
  deadline, highlight timers.
- Normal-mode paste-burst translation is not applied to client input (pastes
  are refused anyway).
- Clients freeze on the last snapshot while the daemon shuts down, then exit
  when the socket closes.

## Open Questions

- Whether skipped `Instant` fields are missed enough to send as ages.
- `data_rate` changes the torrent manager tick rate; it stays daemon-owned.
