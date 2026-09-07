# External image acceptance

This opt-in test downloads a caller-supplied single-file image from an independent
WebTorrent browser seed into an empty native manager destination using BEP 9 and
WebRTC. It then restarts the native manager, rechecks the downloaded file, and
seeds a fresh browser using the production Wasm OPFS backend. The browser closes
and reopens OPFS before exporting the bytes for SHA-256 verification.

The source image and expected SHA-256 must come from an authorized publisher.
External images, torrent files, client bundles and run outputs stay outside the
committed fixtures. No distribution names or images are embedded in the tests.

Prepare the browser dependencies and storage bindings with `npm ci` and
`npm run test:storage` in `web`. Obtain an independent browser client bundle
(the exercised package is `webtorrent@3.0.21`, `dist/webtorrent.min.js`).
Create a JSON configuration using absolute paths:

```json
{
  "source": "/scratch/source.iso",
  "torrent": "/scratch/image.torrent",
  "sha256": "<publisher SHA-256>",
  "tracker": "wss://<public WebTorrent tracker>",
  "output": "/scratch/new-run-directory",
  "client_dist": "/scratch/package/dist/webtorrent.min.js",
  "browser_profile": "/scratch/new-browser-profile"
}
```

The output directory must not exist. Run from the worktree root:

```sh
IMAGE_ACCEPTANCE_CONFIG=/scratch/config.json cargo test --lib \
  --no-default-features --features webtorrent external_image_roundtrip \
  --locked -- --ignored --nocapture
```

DHT and PEX are compiled out. The test supplies no incoming native listener,
HTTP/UDP trackers or web seeds, and checks that all reported payload peers are
WebRTC. `results.json` records the completed phases. The output also contains the
native image and browser export; the test intentionally retains them for review.

This exercises a controlled browser peer discovered through a public tracker.
It does not establish connectivity across different NATs or qualify a complete
browser-hosted Superseedr manager/application runtime.

## Initial run before fixes: 2026-09-05

The input was the publisher's 755 MiB installer image (791,674,880 bytes),
with 3,020 pieces. The original torrent info hash was
`481b6e3617be4c88f96cb25e47c9d8272130071e`. The controlled peer ran
WebTorrent 3.0.21 in Chromium, with public signaling through
`wss://tracker.webtorrent.dev`. An earlier attempt through a different public
tracker did not establish a usable transfer; this is not a general NAT or public
tracker reliability qualification.

| Check | Result |
| --- | --- |
| Empty-destination native magnet download, RTC only | Passed; 791,674,880 payload bytes, 3,020 verified pieces, 389.8 seconds including final checksum |
| Native manager restart and disk recheck | Passed; 3,020 pieces verified before seeding |
| Sustained native RTC seeding into browser OPFS | Failed; repeated request timeouts and reconnects; the initial attempt was stopped after reporting 327,057,408 uploaded bytes |
| Bounded seeding reproduction from the verified file | Failed at its 120-second phase deadline; 12 dropped upload requests logged and a browser request timeout observed |
| Production OPFS full-image write, close/reopen, export in a disk-backed browser profile | Passed; full checksum matched; one cached sync handle |
| Full-image OPFS in an ephemeral browser context | Failed; writes reported success but the persisted file was truncated |

The source, completed native download, and persistent-profile OPFS export shared
the publisher's SHA-256:

```
65273beed27b2df543b68b65630ba525cfbad8df2b12035732b2dff87d6664e7
```

Publisher artifacts: [torrent](https://cdimage.debian.org/debian-cd/13.6.0/amd64/bt-cd/debian-13.6.0-amd64-netinst.iso.torrent),
[image](https://cdimage.debian.org/debian-cd/13.6.0/amd64/iso-cd/debian-13.6.0-amd64-netinst.iso),
[SHA-256 list](https://cdimage.debian.org/debian-cd/13.6.0/amd64/iso-cd/SHA256SUMS).

## Fixes and regression coverage

### Upload admission and delivery

The initial run exposed a pre-existing manager behavior: it discarded a valid
request when the peer's 16-slot upload-read semaphore was full. Temporary
instrumentation recorded 12 such drops before a browser request timeout. The
same limit and discard behavior exist at baseline `465fa8dc`; this run does not
establish how frequently the problem occurs over TCP.

The manager now retains at most 512 pending upload tasks per peer, including
active reads. Tasks await the existing 16-read semaphore and bounded peer mailbox.
Repeated requests for an already pending block reuse its task. Exceeding the
pending limit reports `Action::UploadQueueFull` to state. State decides to remove
the peer and emits its existing session cancellation and upload cleanup effects. Cancels, pause, shutdown, and peer removal
continue to use the existing upload task ownership. Completion carries a task
identity so an old completion cannot remove a replacement task.

The session retains requests and cancels while the manager mailbox is full and
continues draining manager commands during that wait. Payload delivery awaits
writer capacity and remains cancellable. Upload accounting occurs only after
successful admission to the peer mailbox; it remains an enqueue counter, not a
claim of remote receipt. `state.rs` is unchanged from `465fa8dc`.

Regression tests cover requests beyond the 16-read limit, a saturated peer
mailbox, queued cancellation, duplicate requests, bounded overload with
state-authorized removal, stale completions, a saturated writer, and request/
cancel forwarding and session cancellation under manager mailbox pressure.

### Verified-piece announcements

After the upload fixes, the repeated native download passed all 3,020 pieces and
the publisher checksum in 446.5 seconds. Its seed phase then stopped making
progress at exactly 64 MiB (256 pieces), without request timeouts. The manager
had discarded the remaining `have` announcements when the 256-slot RTC peer
mailbox filled during validation completion. The same unchecked broadcast
handoff exists at baseline `465fa8dc`.

The manager now retains unsent announcements in a set per peer, coalescing
repeated indices. The set contains delivery records from state's verified-piece
effects and is bounded by the torrent's piece count. Every 50 ms, pending peers
retry at most 64 entries each through their existing bounded mailbox. Disconnect
effects discard the old session's pending deliveries. The session also retains
`have` and `bitfield` messages through writer and manager backpressure.
Retransmissions use ordinary `have` wire messages.

The regression reproduces the 3,020-announcement burst into a 256-slot mailbox,
then verifies eventual delivery of every piece, duplicate coalescing, and cleanup
on authoritative removal. Session contracts additionally check both availability
message types under saturated manager and writer queues.

### OPFS progress validation

The ephemeral browser returned 4,294,967,288 for a 4 MiB sync write. The old
backend treated that oversized count as success, although the physical file was
truncated. The exact browser-internal cause remains unproven.

Sync reads and writes now require a positive safe-integer count no larger than
the remaining buffer. Invalid counts reject the operation through the existing
storage error path. Valid short operations continue from the exact returned
offset. Browser contracts exercise zero, negative, fractional, non-finite,
oversized, and the observed unsigned return value for both reads and writes;
they also verify partial transfers, admission release on failure, normal quota
exceptions, and close/reopen durability. Both sync and writable modes pass, with
no more than two cached handles.

The full-image ephemeral repetition now fails explicitly with
`UnknownError: invalid OPFS write count`; it does not reach successful completion
or export. A reported quota estimate is not proof that every physical write can
succeed. This fix detects failure rather than increasing browser storage limits.

### Local evidence

Original evidence remains in `target/iso-acceptance/report.json`,
`download-and-seed-attempt.log`, `seed-diagnostic.log`, `opfs-control.log`, and
`opfs-persistent.log`. Fixed-run logs use the `fix-` prefix, including
`fix-storage-tests.log`, `fix-ephemeral.log`, `fix-native-tests.log`, and
`fix-all-tests.log`. The final validation counts and full reseed results are recorded below.

## Final acceptance after fixes

The corrected download passed in `fixed-roundtrip-1788634596`. After the
announcement fix, the successful reseed used that checksum-verified native file
through `native_seed`, with a new manager, destination and persistent browser
profile in `fixed-seed-1788635566`. These are separate test executions; the
intermediate seed that exposed lost announcements was stopped and retained as
evidence.

| Check | Final result |
| --- | --- |
| Native magnet download over public-tracker WebRTC | 791,674,880 bytes; 3,020 verified pieces; publisher SHA-256 matched; 446.5 seconds |
| Fresh manager recheck and complete reseed into browser OPFS | 791,674,880 uploaded bytes; 3,020 pieces; passed in 467.7 seconds including final integrity checks |
| OPFS close/reopen and physical file size | 791,674,880 bytes; one cached sync handle; zero pending operations |
| Exported browser image | Publisher SHA-256 matched |
| Payload transport | WebRTC; TCP and uTP counts zero; no request timeouts in the successful seed |
| Ephemeral browser storage exhaustion reproduction | Explicit invalid-write-count error; no false completion or export |
| Library tests | 2,018 WebTorrent-only and 2,242 all-features tests passed |
| Browser contracts | Both RTC contracts and both OPFS modes passed |
| Strict lint and formatting | Native all-targets/all-features, WebTorrent-only, Wasm, formatting and diff checks passed |
| State freeze | `src/torrent_manager/state.rs` unchanged from `465fa8dc` |

The native manager and browser both shut down successfully after verification.
`target/iso-acceptance/fix-report.json` combines the verified phase results and
artifact paths. The final seed log is `fix-seed-final.log`; final native tests and
lint logs contain `final` in their names. The full image and browser export are
retained for inspection.

### Focused repetitions

An optional `native_seed` config field names the previously checksum-verified
native download. It runs only the recheck/seeding phase. An optional
`phase_timeout_seconds` bounds each transfer phase. The overall timeout allows
both phases plus four minutes for setup and cleanup.

For an independent full-file storage check, use a phase JSON produced by the
native test, set `mode` to `storage`, and provide a new `export` file path.
Run `node web/tests/rtc-image.mjs /absolute/path/to/config.json` from the worktree
root. An optional `browser_profile` path selects a disk-backed persistent
Chromium profile; without it, Playwright uses an ephemeral context. The profile
and export are test artifacts retained for inspection.
