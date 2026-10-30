# Fuzzing

`superseedr` has a narrow `cargo-fuzz` setup for protocol parser surfaces that
should reject malformed input without panicking, aborting, or hanging. It also
includes reducer targets for action/effect logic that can be fuzzed without
starting the full application runtime.

## Setup

Install the fuzz runner and nightly toolchain:

```sh
cargo install cargo-fuzz
rustup toolchain install nightly
```

List available targets:

```sh
cargo fuzz list
```

## Targets

- `utp_packet_decode` drives the BEP 29/uTP packet decoder.
- `utp_packet_roundtrip` builds valid uTP packets from fuzz bytes and checks
  encode/decode invariants.
- `tcp_message_parse` drives the TCP peer message parser.
- `tcp_message_roundtrip` builds peer messages from fuzz bytes and checks
  encode/decode invariants.
- `torrent_file_parse` drives torrent metainfo bencode parsing.
- `torrent_info_parse` drives raw `info` dictionary parsing.
- `krpc_message_decode` drives DHT KRPC bencode/message decoding.
- `krpc_compact_decode` drives compact peer/node decoding for IPv4 and IPv6.
- `krpc_query_roundtrip` builds KRPC queries from fuzz bytes and checks that
  encoded queries decode as queries.
- `dht_lifecycle_reduce` drives DHT lifecycle actions and checks exact emitted
  effects for each action variant.

## Seeds And Dictionaries

The suite uses protocol dictionaries under `fuzz/dictionaries/`.

Generated seed corpora are ignored under `fuzz/corpus/`; regenerate them with:

```sh
python3 scripts/fuzz_seed_corpus.py
```

## Smoke Runs

Use the wrapper for quick local validation. It regenerates seed corpora and
passes the matching dictionary to each target:

```sh
bash scripts/fuzz_smoke.sh
```

Override the run count with `FUZZ_RUNS`:

```sh
FUZZ_RUNS=10000 bash scripts/fuzz_smoke.sh
```

Individual targets can still be run directly:

```sh
cargo +nightly fuzz run utp_packet_decode -- -runs=1000 -dict=fuzz/dictionaries/utp_packet_decode.dict
```

Longer runs belong in scheduled jobs or dedicated local soaks:

```sh
cargo +nightly fuzz run utp_packet_decode
```

Artifacts, corpora, and coverage output are ignored under `fuzz/`. Build output
uses Cargo's normal ignored `target/` directory.

## Scheduled campaigns

`.github/workflows/nightly.yml` builds the default-feature release library test
harness once on Ubuntu 24.04 with Rust 1.95.0. Eight independent jobs then run
`scripts/nightly_properties.py`. Compilation and cache publication finish before
campaign execution, so a test timeout cannot prevent the compiled cache being
saved. The ordinary Rust workflow remains responsible for the feature/platform
matrix, executable tests, and doctests.

The property inventory and budgets live in `CAMPAIGNS` in that script:

- `baseline`: all non-ignored library tests, 256 cases per property. This also
  catches newly added tests before they receive an explicit deep campaign.
- `state`: 13 torrent properties, one million cases each. Covers piece/block
  completion, v1/v2/hybrid shapes, lifecycle, churn, network faults/reordering,
  choking, fairness, rarest-first selection, and state invariants.
- `state-scale`: the existing huge-swarm rarest-first property, one million cases.
- `dht`: four lookup, demand scheduler, planner reducer, and planner selection
  properties, one million cases each.
- `storage`: three packed-bitfield/reset and rarity-rebuild parity properties,
  one million cases each.
- `telemetry`: activity grouping and publication parity, one million cases each.
- `assignment`: small-fixture refill and stable-sort parity, one million cases
  each, including both priority modes in every stable-sort case.
- `assignment-scale`: two explicitly ignored properties, 2,000 cases each,
  always using large fixtures selected from 32,767, 32,768, 33,080, and 65,536
  pieces. They call the same assertion helpers as the small campaign. Existing
  deterministic large-fixture tests still run in ordinary CI.

Each campaign has a 35-minute execution deadline inside a 40-minute job, with
shrinking limited to 60 seconds per failing case. A
missing test, ignored expected test, failed assertion, or timeout fails the
campaign. Other campaigns continue. Receipts record the source commit, tracked dirty state, executable SHA-256, command,
case budget, random seed, elapsed time, passed tests, and incomplete tests.
`PROPTEST_RNG_SEED` can be supplied to repeat a campaign; exact minimized inputs
in regression files are preferable for reproduction, particularly for tests
that also use randomized hash iteration or wall clocks. A case count is a
requested successful-case budget, not a coverage percentage or an execution
count including rejected/shrunk inputs.

Run a local smoke campaign against a compiled library test executable:

```sh
python3 scripts/nightly_properties.py assignment /path/to/property-tests --cases 1000
python3 scripts/nightly_properties.py assignment-scale /path/to/property-tests --cases 100
```

Without `--cases`, the script uses the full scheduled budget. Use Kyoto for the
Rust compilation in workspaces where that policy applies. The executable must
be built from the source under test; a compile-only result is not a campaign
pass. Register new properties in the appropriate campaign and verify exact-name
discovery after moving or renaming tests.

A separate job builds all targets from `fuzz/Cargo.toml` with nightly Rust,
AddressSanitizer, and cargo-fuzz 0.13.1. Cargo build concurrency is limited to one
to avoid linking multiple instrumented binaries simultaneously; build logs
include peak resource measurements. The protocol job overrides
`CARGO_PROFILE_RELEASE_LTO=false` and `CARGO_PROFILE_RELEASE_STRIP=none` for both
build and run: workspace release LTO can produce undefined `__sancov_gen_`
symbols ([cargo-fuzz issue 384](https://github.com/rust-fuzz/cargo-fuzz/issues/384)).
The ordinary release and property harness profiles are unchanged. The job then runs
`scripts/nightly_protocols.py`. Each of the ten targets receives 60 seconds of
mutation time, a 10-second individual-input timeout, a 2 GiB RSS limit, and a
64 KiB maximum generated input. An outer process deadline includes 120 seconds
of startup/shutdown allowance. Failures do not skip later targets. Inputs over
64 KiB and long-running end-to-end network/runtime behavior are outside this
campaign's qualification.

Evolved protocol corpora are restored across nights and saved even after a
failed target; crash inputs also remain in the uploaded artifacts.

Generated seeds include valid v1, multifile, v2, and hybrid metainfo, each TCP
roundtrip variant, each KRPC query and DHT lifecycle variant, and truncated or
boundary-sized protocol records. The KRPC oracle compares query kind,
transaction/node identity, and all arguments; merely decoding as a query is
insufficient. Parser rejection seeds remain intentional alongside valid seeds. Torrent-file and
raw-info parsing reject bencode deeper than 64 list/dictionary containers before
entering the recursive deserializer. An iterative framing scan skips byte-string
payloads and checks length arithmetic; this also bounds subsequent value traversal
and destruction. Generated seeds cover the limit, one past it, and the deeply
nested shape found by the October 30 nightly run. This intentionally rejects
otherwise well-formed metainfo above the nesting limit.

All campaign logs, JSON receipts, Proptest regression files, and protocol crash
artifacts/corpora are uploaded for 14 days. The previous automatic regression PR
was removed: timeouts and build failures do not establish a discovered bug, and
parallel jobs need their evidence preserved independently. Review and minimize
an actual failing input before adding a permanent regression.

## October 2026 review and coverage tradeoffs

The October 18 and 19 runs exhausted the old shared 60-minute deadline. Both
completed 2,402 tests with eight ignored; assignment stable-sort parity was the
only unfinished test. Cold compilation consumed about 15–16 minutes. The old
workflow never executed the separate cargo-fuzz targets.

The old assignment generator selected a 33,080-piece fixture in roughly 1/32
cases: about 31,250 large cases per property at the nightly budget. The new
2,000-case scale campaign deliberately samples fewer large randomized
combinations, while covering additional size boundaries and retaining all
assertions. Small randomized coverage remains at one million cases. Fixture
setup also removes pending pieces in one pass rather than repeatedly scanning
the entire queue; a regression test compares this setup with the original
incremental operation.

These are initial budgets, not measured guarantees of hosted-runner completion.
Use the first full scheduled receipts to adjust expensive campaigns without
silently lowering the general property budget. The suite does not establish
exhaustive state-space coverage. In particular, network-fault stories have
bounded lengths, lifecycle reducer fuzzing checks individual action effects,
and browser/WebRTC, filesystem publication, and full live-swarm recovery still
rely on their dedicated tests and integration qualification.
