**Goal**
Move forward from `origin/main` while keeping the old torrent-manager-owned peer flow and selectively porting the internal DHT work from the `dht` branch.

**Why**
The current `dht` branch carries the committed global peer manager architecture from `8cf0a83` (`feat: add authoritative global peer manager`).

That architecture changed peer discovery and admission flow substantially:
- discovered peers are emitted upward
- admission is centralized
- the app loop and shared event channel become part of the hot path

Recent investigation showed that this creates a real high-volume bottleneck during metadata recovery:
- one-peer-at-a-time candidate delivery
- lossy `try_send` on the manager event path
- app-loop admission pressure under sustained DHT/tracker candidate bursts

`origin/main` does not use that architecture. There, torrent managers still own outgoing peer admission and dialing.

**Intent**
Do not keep iterating on the centralized peer-manager architecture in this branch.

Instead:
1. create a fresh branch from `origin/main`
2. preserve `origin/main`'s TM-owned peer management model
3. port only the DHT/runtime improvements that are still desired
4. explicitly avoid reintroducing the global peer manager unless there is a later separate design effort for it

**Expected Branching Strategy**
Start from:
- `origin/main`

Then selectively port DHT-related work from `dht`, likely as cherry-picks or manual ports, not a full merge.

Likely candidates:
- `190c373` `feat: add dht service boundary`
- `1786c09` `feat: expose dht health in status snapshots`
- `46604e3` `feat: add routing cache to internal dht backend`
- `390b783` `feat: add dht announce support to internal backend`
- `cda330a` `feat: announce resumed torrents through dht service`
- `f035684` `Switch DHT boundary to new runtime`
- `78e180c` `Fix internal DHT query interoperability`
- `08b12f4` `Fix internal DHT lookup lifecycle`
- `b2d8d87` `Fix internal DHT inflight query cleanup`
- `f277e99` `Improve internal DHT parity and stability`

These should be reviewed one by one during porting, because some may still contain assumptions from the peer-manager branch context.

**What Must Be Preserved**
- TM-owned outgoing peer admission and dialing
- no global peer-manager dependency in the app loop
- no one-peer-per-event high-volume candidate path through `App`

**Known Risks During Port**
- conflicts in:
  - `src/app.rs`
  - `src/main.rs`
  - `src/torrent_manager/manager.rs`
- some DHT service integration points may need manual adaptation because the surrounding app/runtime wiring diverged after `origin/main`
- any discovery work that assumed centralized peer admission should be rejected or rewritten to fit TM-owned peer flow

**Acceptance Criteria**
- internal DHT runtime and parity fixes are present on the new branch
- peer management remains local to each torrent manager
- no `src/global_peer_manager.rs`
- no app-loop peer-candidate admission path
- metadata recovery does not depend on centralized peer-manager batching

**Execution Notes**
- prefer selective cherry-pick and manual porting over revert-heavy history surgery on the current branch
- keep each imported DHT slice buildable and testable
- re-run targeted DHT deterministic tests and live metadata recovery checks after each major port step
