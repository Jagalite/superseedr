# V2 Identity Lossiness Review

## Summary
This note captures the review and discovery work around the current 20-byte `info_hash` model, especially for pure v2 torrents. The main conclusion is that the broad lossy-identity issue is pre-existing and architectural. It should not be attributed to the recent files-panel or metrics-tick changes.

## Scope
- Review whether the current branch introduced a new v2 "20-byte lossy" tracker bug.
- Document what the code currently does for v1, hybrid, and pure v2 identities.
- Record what the local integration harness proves and what it does not prove.
- Outline the architectural direction without turning this branch into a large identity refactor.

## Current Code Findings
1. `TorrentManager::from_torrent` currently derives one `info_hash` byte vector in `src/torrent_manager/manager.rs`.
   - v1: SHA-1 of `info_dict_bencode` (`20` bytes).
   - hybrid: SHA-1 of `info_dict_bencode` (`20` bytes).
   - pure v2: SHA-256 of `info_dict_bencode`, truncated to `20` bytes.

2. Tracker announce code in `src/tracker/client.rs` treats that single `hashed_info_dict` value as the tracker-facing identity for both transports.
   - HTTP announce percent-encodes the bytes directly into `info_hash`.
   - UDP announce copies the bytes into the fixed `20`-byte announce field.

3. The current architecture therefore conflates:
   - internal torrent identity,
   - DHT/tracker/wire-facing identity,
   - UI/control-plane torrent keying.

4. The broad lossy-identity problem already exists before the recent files-panel changes.
   - Pure v2 identity is collapsed in `src/torrent_manager/manager.rs` before tracker code sees it.
   - The branch changes around file activity batching do not introduce that collapse.

## Review Outcome
1. The review concern is directionally valid at the architectural level:
   - a single anonymous `Vec<u8>` `info_hash` is not a sufficient long-term identity model for v1/v2/hybrid.

2. The review concern is too broad if interpreted as "this branch introduced the 20-byte lossiness bug."
   - The branch under review did not create the underlying pure-v2-to-20-byte collapse.
   - That behavior is already present in `src/torrent_manager/manager.rs`.

3. Hybrid handling should be considered separately from pure v2.
   - In the current code, hybrid torrents already use the v1 SHA-1 path.
   - That is not the same failure mode as pure v2 truncation.

## Evidence From Local Test Infrastructure
1. The checked-in `integration_tests/torrents/v2/single_4k.bin.torrent` fixture is a real pure v2 torrent.
   - `meta version = 2`
   - no `pieces`
   - has `file tree`

2. The checked-in `integration_tests/torrents/hybrid/single_4k.bin.torrent` fixture is hybrid.
   - `meta version = 2`
   - has `pieces`
   - has `file tree`

3. The local integration tracker in `integration_tests/docker/tracker.py` is HTTP-only and accepts whatever `info_hash` byte string it receives.
   - It does not validate v1 vs v2 semantics.
   - It proves interoperability with the harness, not protocol correctness against arbitrary trackers.

4. The cluster-manifest tooling is currently `btih`/SHA-1 oriented in `integration_tests/cluster_cli/manifest.py`.
   - `torrent_info_hash_hex(...)` computes SHA-1 of the top-level `info` dictionary.
   - `magnet_info_hash_hex(...)` only parses `urn:btih:`.

5. Additional local interop testing with qBittorrent/libtorrent reportedly worked with qBittorrent-generated pure v2 torrents.
   - That is strong evidence that the current 20-byte path interoperates in the tested ecosystem.
   - It is not proof that every tracker-facing protocol path is generally correct for pure v2.

## Architectural Conclusions
1. The long-term fix is not "switch everything to 32 bytes everywhere."
   - Different external protocols may still expect different identifiers.
   - Existing control-plane and integration paths are currently built around 20-byte assumptions.

2. The real fix is to stop using one ambiguous `info_hash` representation for every layer.
   - Introduce an explicit torrent identity model.
   - Preserve the real v2 identity for pure v2.
   - Preserve both v1 and v2 identities for hybrid.
   - Make protocol adapters choose the appropriate identity intentionally.

3. This architectural cleanup should be handled in a dedicated follow-up, not as incidental scope in files-panel or metrics work.

## Immediate Guidance For Current Branches
1. Do not attribute the broad 20-byte lossy identity issue to the recent files-panel batching changes.
2. Do not try to "fix" this by blindly forcing 32-byte identities through the existing app, tracker, magnet, and control paths.
3. Keep branch-local fixes scoped to the branch's own regressions unless there is a direct new correctness issue introduced by that branch.

## Follow-Up Work
1. Add focused tracker-client tests that assert the exact announce payload for:
   - v1
   - hybrid
   - pure v2

2. Decide and document explicit pure-v2 UDP policy.
   - supported with a defined identifier model, or
   - rejected/skipped intentionally

3. Introduce a typed torrent identity abstraction instead of a single raw `Vec<u8>`.

4. Audit callsites that currently assume `btih`/SHA-1-only identity handling.
   - tracker client
   - DHT lookup keying
   - magnet parsing/generation
   - CLI/control surfaces
   - integration harness manifests

## Non-Goals For This Note
- This is not an implementation plan for a full v2 identity refactor.
- This does not claim that the current pure v2 tracker behavior is universally correct.
- This does document that the main identity-lossiness concern predates the recent files-panel work and should be treated as a separate architectural track.
