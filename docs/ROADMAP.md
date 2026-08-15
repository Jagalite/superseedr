# Roadmap
This document is a high-level guide to the direction of superseedr.
It is intentionally stable but flexible as implementation details evolve.
For specific tracking, use repository issues and labels.

## Status Baseline (from changelog)
The roadmap now reflects features shipped through `v1.0.1`.

### Shipped
- `v1.0.0`: Integrated RSS workspace, advanced RSS filtering, and safer high-impact confirmation flows.
- `v1.0.1`: RSS duplicate-filter guardrails and follow-up RSS UX/readability fixes.
- `v0.9.38`: Semantic theme system, new built-in themes, theme cycling, and broader theme effects.
- `v0.9.29` to `v0.9.38`: Major TUI architecture and layout/table behavior refactors, including smarter column visibility behavior.
- `v0.9.30`: BitTorrent v2/hybrid support, merkle verification, and related integration test coverage.
- `v0.9.35`: JSON state dump/export foundation for external integrations.
- `v0.9.36`: Peer activity visualization redesign improvements.

## Big Features
- `[Shipped]` **RSS Feed Support**
- `[Planned]` **Config Screen Redesign**: Modernize and refactor config management for more complex user inputs.
- `[Partially Shipped]` **Advanced Torrent Management**: Multi-select, bulk actions, and persistent torrent tags are available; broader tag-driven filtering remains extensible.
- `[Partially Shipped]` **User Adjustables**: Continue improving user control over visible columns and auto behaviors.
- `[Partially Shipped]` **Alternative and Custom Themes**: Extend beyond built-ins with full user-defined theme packs.
- `[Shipped]` **Internal TUI Architecture Refactor**
- `[Planned]` **Persistent Data Across Sessions**: Persist peer/system history and support reputation/blocking logic.
- `[Partially Shipped]` **Scriptability and CLI Enhancements**: Build on JSON status export with richer control surfaces.
- `[Planned]` **Log Levels and TUI**: Runtime log level controls and in-app log viewer.
- `[Planned]` **User Configurable Layouts**: Saveable/reconfigurable layouts.
- `[Partially Shipped]` **Peer Stream Redesign**: Continue expanding stream visual encoding and controls.
- `[Planned]` **Fully Asynchronous Validation**: Complete non-blocking validation/revalidation pipeline work.
- `[Planned]` **Peer Churn Overload Management**: Harden behavior for very large swarms and peer churn.
- `[Partially Shipped]` **Integration Testing**: Continue scaling interop coverage and CI automation.

## Roadmap to v1.5
`v1.0` shipped core usability and stability milestones.
The `v1.1` to `v1.5` window focuses on finishing advanced UX and operator controls on top of the current baseline.

Priority themes:
- Advanced torrent management workflows (multi-select, bulk actions, grouping/tagging).
- Config UX overhaul (structured inputs, validation, grouped sections).
- Logging observability inside TUI (log widget and runtime verbosity controls).
- Validation pipeline hardening (async/revalidation progress and resilience).
- Expanded interop testing depth and CI confidence gates.

## Future (v2.0+)
Longer-term work targets deeper networking and operational control:
- Persistent long-term stats, peer reputation, and auto-blocking.
- Headless/scriptable control surface and richer CLI automation.
- User-configurable/saved TUI layouts.
- High-churn peer management at large scale.
- Networking reachability/features beyond shipped TCP+uTP transport (IPv6,
  UPnP, hole punch, DHT search).

# Detailed Roadmap Steps

## Phase: 1.1 to v1.5
**Goal:** Complete advanced management and observability on top of the shipped v1.0 baseline.

### Advanced Torrent Management
- **phase: 1.1 to v1.5** | Multi-select State - internal logic to track multiple selected rows | [Issue #____]
- **phase: 1.1 to v1.5** | Bulk Actions - apply start, stop, and delete to selection context | [Issue #____]
- **phase: 1.1 to v1.5** | Grouping/Tagging - associate torrents with persistent tags for management flows | [Issue #307]

### Config Screen Redesign
- **phase: 1.1 to v1.5** | Input Field Refactor - support complex field types (dropdowns, toggles) | [Issue #____]
- **phase: 1.1 to v1.5** | Categorized Views - sectional layout for config groups | [Issue #____]
- **phase: 1.1 to v1.5** | Field Validation - immediate visual feedback for invalid inputs | [Issue #____]

### Log Levels and TUI
- **phase: 1.1 to v1.5** | Structured Logging - level-based logging (INFO, DEBUG, WARN) across modules | [Issue #____]
- **phase: 1.1 to v1.5** | Log Widget - scrollable in-app view to tail recent logs | [Issue #____]
- **phase: 1.1 to v1.5** | Runtime Verbosity - user setting to change log level without restart | [Issue #____]

### Fully Asynchronous Validation
- **phase: 1.1 to v1.5** | Async Hashing - verification without blocking primary UI/input loop | [Issue #____]
- **phase: 1.1 to v1.5** | Validation Progress - granular progress events for UI and status output | [Issue #____]
- **phase: 1.1 to v1.5** | Revalidation Logic - robust forced re-check handling for existing data | [Issue #____]

### Integration Testing Expansion
- **phase: 1.1 to v1.5** | Matrix Expansion - broaden client/version interop matrix coverage | [Issue #____]
- **phase: 1.1 to v1.5** | CI Stability - auto-run integration suites with reliable artifacts and triage logs | [Issue #____]
- **phase: 1.1 to v1.5** | Regression Scenarios - codify RSS and UI edge-case regressions in automated tests | [Issue #____]

### Scriptability and CLI Enhancements
- **phase: 1.1 to v1.5** | CLI Control Surface - pause/resume/list/control torrents from CLI paths | [Issue #____]
- **phase: 1.1 to v1.5** | JSON Output Expansion - richer machine-readable status for automation | [Issue #____]
- **phase: 1.1 to v1.5** | Headless Mode Foundations - separate core runtime from TUI lifecycle | [Issue #____]

---

## Phase: 2 - v2.0+
**Goal:** Advanced networking, persistent analytics, and fully customizable operations.

### Persistent Data Across Sessions
- **phase: 2 - v2.0+** | Stats Database - local storage for long-term metrics and trends | [Issue #____]
- **phase: 2 - v2.0+** | Peer Reputation Logic - track peer behavior over time | [Issue #____]
- **phase: 2 - v2.0+** | Peer Blocklist - automated or assisted peer blocking from reputation data | [Issue #____]

### User Configurable Layouts
- **phase: 2 - v2.0+** | Layout Serialization - save and restore panel positions/sizes | [Issue #____]
- **phase: 2 - v2.0+** | Interactive Resize - keybindings for pane resizing workflows | [Issue #____]
- **phase: 2 - v2.0+** | Layout Presets - user-selectable named layout profiles | [Issue #____]

### Peer Churn Overload Management
- **phase: 2 - v2.0+** | Connection Capping - active vs. pending connection caps | [Issue #____]
- **phase: 2 - v2.0+** | Aggressive Pruning - disconnect low-value peers under load | [Issue #____]
- **phase: 2 - v2.0+** | 10k Scale Test - profiling and behavior validation with massive peer sets | [Issue #____]

### Networking Feature Parity
- **phase: 2 - v2.0+** | IPv6 - transport capability expansion | [Issue #____]
- **phase: 2 - v2.0+** | UPnP/NAT Traversal - improved reachability and connectivity | [Issue #____]
- **phase: 2 - v2.0+** | DHT Search and Discovery - richer discovery tooling and UX | [Issue #____]
- **phase: 2 - v2.0+** | DHT Ownership - evaluate and stage a first-party DHT runtime to replace `mainline` | See [dht-ownership-plan.md](dht-ownership-plan.md)
