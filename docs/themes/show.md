# Show

Select **Show** in the existing theme picker (or cycle themes with `<` / `>`).
The saved setting is `ui_theme = "show"`.

Show is a native, deterministic sequence of 30 layered scenes. Broad paired-color
fields, moving geometric tracers, background glyph texture, border chases and
spatial typography run from the existing UI effects clock and frame pass.

Each scene lasts 32 steps at 0.4 seconds per step (12.8 seconds); the complete set
repeats after 6 minutes 24 seconds. Transfer traffic does not alter the tempo.
Each scene contains four eight-step phrases: **build, peak, break, return**. The
break lowers the broad color field while keeping its geometric structure visible.
A primary chase, fading echo and counter-chase give the layers distinct motion
on the same step grid. Color pairs change on two-step boundaries; the second half
of the scene reverses their roles.

Background, borders, font palette and pulse share this score. Six typography
patterns distribute color and pulse by active bands, rows, columns, radial rings,
split halves or a unified hit. The native frame cadence determines temporal
smoothness, while the score keeps its tempo even at low frame rates. Use 30 or
60 fps to see the full pulse detail. Power-saving mode retains its existing
on-demand redraw policy.
The score has no audio input.

## Scene set

| # | Background | Palette | Pulse | Font pattern |
|---|---|---|---|---|
| 1 | Prism chase | Cyan / pink | Double hit | Follows active bands |
| 2 | Pulse tunnel | Acid / violet | Snap + decay | Radial rings |
| 3 | Mirror shards | Ice / blue | Local flicker | Alternating rows |
| 4 | Echo chamber | Cyan / pink | Double hit | Radial rings |
| 5 | Checker switch | Acid / violet | Hold + cut | Column chase |
| 6 | Spiral drive | Mint / lilac | Swell + cut | Follows active bands |
| 7 | Wave interference | Ice / blue | Swell + cut | Unified |
| 8 | Honeycomb | Amber / rose | Double hit | Follows active bands |
| 9 | Radar sweep | Mint / lilac | Snap + decay | Radial rings |
| 10 | Diamond lattice | Cyan / pink | Local flicker | Alternating rows |
| 11 | Signal rain | Ice / blue | Double hit | Column chase |
| 12 | Warp grid | Acid / violet | Snap + decay | Split halves |
| 13 | Sine ribbons | Amber / rose | Swell + cut | Follows active bands |
| 14 | Moire weave | Mint / lilac | Swell + cut | Alternating rows |
| 15 | Star aperture | Cyan / pink | Snap + decay | Radial rings |
| 16 | Diamond echo | Acid / violet | Double hit | Radial rings |
| 17 | Binary weave | Ice / blue | Local flicker | Column chase |
| 18 | Pinwheel | Amber / rose | Hold + cut | Follows active bands |
| 19 | Ripple pool | Mint / lilac | Swell + cut | Unified |
| 20 | Circuit traces | Cyan / pink | Local flicker | Follows active bands |
| 21 | Zigzag ladder | Acid / violet | Double hit | Alternating rows |
| 22 | Hourglass | Ice / blue | Snap + decay | Split halves |
| 23 | Woven rings | Amber / rose | Swell + cut | Follows active bands |
| 24 | Rosette | Mint / lilac | Double hit | Follows active bands |
| 25 | Crosshatch | Cyan / pink | Local flicker | Alternating rows |
| 26 | Stepped terraces | Acid / violet | Hold + cut | Follows active bands |
| 27 | Polar checker | Ice / blue | Double hit | Radial rings |
| 28 | Split scan | Amber / rose | Snap + decay | Split halves |
| 29 | Orbit interference | Mint / lilac | Swell + cut | Radial rings |
| 30 | Shutter fan | Cyan / pink | Local flicker | Alternating rows |

## Foreground particle experiment

Particles fire on specific score steps. Build phrases use small cues; peak and
return phrases open up the energetic scenes. The entire break and the end of
each phrase are clear. Cohorts live for less than a second, with fixed spatial
variation and analytic motion; there is no continuous random snowfall.

| Movement | Scenes |
|---|---|
| Mirrored shard volleys | Prism chase, mirror shards, diamond lattice, zigzag ladder, crosshatch |
| Accelerating warp streaks | Pulse tunnel, warp grid, hourglass |
| Curved vortex trails | Spiral drive, pinwheel, orbit interference |
| Sparse radial glints | Echo chamber, honeycomb, ripple pool |
| Drifting wisps | Wave interference, sine ribbons, moire weave, woven rings |
| Directional comets | Radar sweep, signal rain, circuit traces, split scan |
| Radial bursts | Star aperture, diamond echo, rosette, shutter fan |
| Twin fountains | Stepped terraces |
| Tumbling confetti | Checker switch, binary weave, polar checker |

The geometric scenes can fire on every second step at their peak and use larger
return cues. Fluid scenes retain the sparse two-cue arrangement. Colors follow
the current scene pair. Bright heads lead short fading trails above the existing
background texture, while all original UI text and protected surfaces remain
clear. Density scales with the viewport and is capped at one particle mark per
16 cells, up to 320 marks. Heads have priority over tails at intersections.

## Native rendering

Each scene combines a broad color field with supporting geometry: rings and rails
for tunnels, cross-diagonals for shards, spokes for radial scenes, signal traces
for circuits, and counter-moving ribbons for fluid scenes. The supporting layer
uses the opposite palette color and a synchronized offbeat pulse.

Fine glyph texture appears only inside runs of clear background space, with a
blank margin beside UI content. Existing characters, wide-character continuation
cells, combining marks, selected surfaces and reversed/hidden/skipped cells are
preserved. Texture stays below text brightness.

Borders carry saturated chases. Body text gets a separate brightness lift so
local pulses and flicker remain legible. Metric and chart accents pulse toward
white while retaining their color identity; error, warning and success foregrounds
keep their exact colors. Non-base backgrounds remain intact.

The implementation lives in `src/tui/render/show.rs`, called once from the existing
`apply_theme_effects_to_frame` pass. Its private `show/foreground.rs` module
composes particle cues after the background pass using the original clear-space
mask. Theme registration, serialization and the static fallback palette live in
`src/theme.rs`. Existing screens and theme
selection/persistence reducers are reused.

## Verification

```sh
cargo test --locked --lib tui::render
cargo test --locked --lib theme::tests
```

Tests cover the distinctness of all 30 patterns, score cycling, pulse envelopes,
layer coverage, phrase dynamics, typography variation, text contrast, semantic
colors, selection/modifier preservation, wide text, viewport offsets, small
terminals, frame-rate-independent timing, power saving and production screen draws.

For an optional visual review, export frames from the production normal-screen
renderer (120 columns by 40 rows, one attack from each of the four phrases per scene):

```sh
SUPERSEEDR_SHOW_GALLERY=/tmp/show-frames.json cargo test --locked --lib \
  render_native_show_gallery -- --ignored
```

The JSON contains native buffer symbols and RGB colors, not a reimplementation
of the TUI. This export test is ignored during normal test runs.

To export a full 12.8-second native scene at 20 fps, add its one-based index:

```sh
SUPERSEEDR_SHOW_GALLERY=/tmp/show-motion.json SUPERSEEDR_SHOW_MOTION_SCENE=15 \
  cargo test --locked --lib render_native_show_gallery -- --ignored
```
