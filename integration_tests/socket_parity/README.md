# Linux socket parity harness

This harness compares the unrestricted/default network behavior of a pinned main
revision with a descendant branch revision. Both binaries are built sequentially
in one Linux image, then run against the same live torrent input.

Each application run receives a distinct state tree. The harness verifies that
the shared root did not exist before the run and that it has zero entries before
the traced application starts. The live torrent is queued only after that first
startup publishes a status snapshot.

Run it with a public live torrent URL:

```sh
SUPERSEEDR_SOCKET_PARITY_TORRENT_URL='<public-live-torrent-url>' \
  ./integration_tests/socket_parity/run_linux_socket_parity.sh
```

Optional controls:

- `SUPERSEEDR_SOCKET_PARITY_MAIN_REF` defaults to `origin/main`.
- `SUPERSEEDR_SOCKET_PARITY_BRANCH_REF` defaults to `HEAD`.
- `SUPERSEEDR_SOCKET_PARITY_RUN_SECONDS` defaults to `20` seconds per revision.
- `SUPERSEEDR_SOCKET_PARITY_RUN_REPEATS` defaults to two independently fresh runs
  per revision; their observed profile sets are merged before comparison.
- `SUPERSEEDR_SOCKET_PARITY_ARTIFACT_DIR` overrides the ignored artifact directory.

The main reference must be an ancestor of the branch reference. The image records
both resolved commit IDs, its architecture, Linux kernel, OS release, image digest,
and the downloaded torrent checksum.

`strace` records successful `socket`/`accept` creation plus `setsockopt`, `bind`,
`connect`, `listen`, nonblocking `fcntl`/`ioctl`, descriptor duplication, and close.
The normalizer removes file descriptors, timestamps, concrete addresses, and port
numbers. It compares:

1. the set of socket constructors observed across every created socket; and
2. the set of maximal explicit configuration profiles.

Sockets cancelled between creation and their first configuration syscall at forced
shutdown remain counted as `incomplete_at_shutdown`. Short-lived connections that
end before a later option is applied remain in the observed profiles, but are treated
as partial lifecycles when the same constructor's operation superset was also observed.
Raw traces, raw-profile differences, and per-profile occurrence counts remain available
for review. Partial lifecycles and occurrence counts are diagnostic only because live
peer and tracker activity is intentionally nondeterministic.

Artifacts are written under `integration_tests/artifacts/socket-parity-<timestamp>/`.
`comparison.json` is the summary gate; a static constructor or profile difference
causes the harness to exit nonzero.
