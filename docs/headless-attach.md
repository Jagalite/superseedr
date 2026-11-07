# Headless Mode and `superseedr attach`

Status: proposal, phase 1 in progress.

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
            └─── Unix socket next to the lock file (0600) ───┘
```

- Only data crosses the socket. No keystrokes, no rendered frames.
- Each client renders in its own process, so selection, screen, sort, and theme
  are naturally per-client. No shared view state on the daemon.
- This mirrors shared-config cluster mode: the daemon acts like a leader, an
  attached client acts like a follower that has no engine of its own.

Normal mode is unchanged: one process, the local terminal, no socket, no
serialization.

## Wire Protocol (phase 1)

One-way: daemon → client. A Unix socket rather than a FIFO, so each client gets
its own stream.

```
Header   { protocol, daemon_version }
Snapshot { ... }        # full, on connect
Snapshot { ... }        # full, every stats tick (1s)
...
```

- Length-prefixed frames.
- The client checks `protocol` and disconnects with a clear message on mismatch.
- The daemon serializes once per tick and shares the bytes with every client.
- Each client has a latest-wins slot: a slow client skips stale snapshots, and
  the engine never waits on a socket.
- A write error (client gone) drops that client.

Full snapshots keep every message self-contained. If measurement on a large
catalog shows they are too big, make only the heavy sections incremental
(append-only histories, changed torrents) with periodic full snapshots. Keep the
snapshot split into sections so that change stays local.

## Read-Only Client Rules

- Allowed locally: navigation, selection, search, help, sorting, chart and graph
  modes, theme (not persisted).
- Refused with a "read-only client" message: every engine-writing effect
  (control requests, config apply, RSS actions, pasted adds, file browser on the
  daemon's filesystem).
- `q` detaches. `superseedr stop-client` still stops the daemon.

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

- Torrents: `torrents`, `torrent_list_order`.
- Rates and histories: `avg_download_history`, `avg_upload_history`,
  `network_history_state`, `activity_history_state`, `avg_disk_*_bps`,
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

Most of these types are plain data or already `Serialize` (`TorrentMetrics`,
`PeerInfo`, `Settings`, `DhtStatus`, the persisted history states). The rest
need derives. `Instant` fields (recent file activity, disk op timestamps,
highlight timers) are skipped in phase 1; they only drive short animations.

## Open Questions

- Whether skipped `Instant` fields are missed enough to send as ages.
- `data_rate` changes the torrent manager tick rate; it stays daemon-owned.
- Where the socket lives in shared-config mode (next to the shared lock).
