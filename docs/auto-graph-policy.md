# AUTO graph: live burst detection

AUTO follows meaningful rises in combined download and upload throughput. It
keeps related spikes together within a rolling ten-minute maximum. It no longer
scores the shape of saved history to choose a range.

## Detection and display policy

`src/telemetry/auto_graph.rs` consumes live one-second samples and maintains a
constant-size detector and optional burst period:

1. Calibrate from the first three live samples. Steady traffic already present
   at startup becomes the reference; it does not manufacture a new burst.
2. Use a three-sample median to reject isolated high/zero samples, then an EWMA
   with weight 0.35 to detect a sustained rise.
3. The entry margin is the largest of 50% of the reference rate, four times the
   smoothed absolute deviation, and 4 KiB/s. Both the median and EWMA must exceed
   the reference plus this margin to start a burst.
4. Continue the burst while the median exceeds the reference plus half the
   entry margin. This lower continuation threshold prevents repeated entry and
   exit near one boundary. Freeze the reference throughout the burst and its
   cooldown, so a larger peak cannot erase smaller subsequent spikes.
5. Outside a burst, learn the reference and clipped absolute deviation with
   weight 0.02, only from samples below the continuation threshold. This avoids
   absorbing a candidate rise into the reference before detection.
6. Remember the burst's start and last active sample. Nearby spikes extend the
   same period across quiet gaps of up to two minutes. Median-filter latency
   adds roughly one second to the observed end of a burst.
7. Frame the detected span with about 20% space for context: 1m up to 48 seconds,
   5m up to 240 seconds, then 10m. The range only widens on active samples and
   never shrinks within the same period. A longer burst stays at a rolling 10m.
8. After the cooldown, return to 1m. Expiring history, restored data, or old peaks
   cannot create a new burst because none of them are detector inputs.

The existing display controller reevaluates every five seconds and widens at
most one marker per twenty seconds. It can shrink directly to the live default
on its next evaluation. Detector updates continue while a manual range is
selected, so returning to AUTO can frame an already detected burst. Manual
ranges through 1y remain available.

Duplicate timestamps are ignored by the detector. Clock rollback or a sampling
interruption longer than two minutes starts fresh calibration. Shorter sampling
gaps retain the last three real observations. Signal and baseline smoothing use
elapsed-time weights (`1 - (1 - alpha)^seconds`) so repeated delayed ticks still
detect sustained rises. The median still needs real samples; missing seconds
are not filled with copies of the next observation.
The normal native and browser second-tick paths share this implementation.

## Why EWMA

[EWMA](https://www.itl.nist.gov/div898/handbook/pmc/section3/pmc324.htm) and
[CUSUM](https://www.itl.nist.gov/div898/handbook/pmc/section3/pmc323.htm) are
established tools for detecting shifts in a process. Here they are engineering
building blocks, not a claim of textbook statistical guarantees for correlated
network traffic. The median filter, adaptive reference, margins, grouping, and
display rules above are explicit application policy.

Reproduce the fixed-step detector comparison with:

```sh
python3 scripts/compare_auto_graph_detectors.py
```

Each detector sees the same 1,080 synthetic traces: ten random seeds, three rate
scales (0.1, 5, and 50 MB/s), 0/10/20% uniform jitter, four burst patterns with
1.6x–20x heights, and 30/90/150-second quiet gaps. Traces also contain isolated
high and zero outliers. There are 2,430 labeled sixty-second bursts per detector.

Both candidates share filtering, reference learning, grouping, and framing. The
reference CUSUM uses `max(0, sum + normalized_rise - 0.5)` with decision threshold
4; EWMA uses the entry rule above. This is a comparison of these two parameter
settings, not an exhaustive search for their best possible tuning.

| Metric | EWMA | CUSUM |
| --- | ---: | ---: |
| Detected labeled bursts | 2,430 / 2,430 | 2,430 / 2,430 |
| Median detection delay | 1s | 1s |
| 95th-percentile delay | 5s | 6s |
| Maximum delay | 13s | 12s |
| False active seconds outside transitions | 0 | 0 |
| Total target-range changes | 3,324 | 3,330 |
| Current-group burst sample coverage | 99.998% | 99.998% |

Delay is measured from the start of each labeled burst to its first active
detection, including continued activity in an existing group. False-activity
counts exclude a three-second allowance after a labeled burst for filtering.
Coverage is the fraction of the current group's burst samples from the last ten
minutes contained in the target window, measured during labeled bursts. Total
range changes include expected widening and cooldown resets. These target-window
metrics exclude the display controller's five-second evaluation and twenty-second
widening delay; Rust integration tests cover the actual display transitions.

EWMA was selected for its slightly better typical delay and simpler detector
state. These results cover the generated step/spike traces, not every possible
traffic distribution, gradual ramp, or a captured live session.

## Regression checks

```sh
cargo test --locked --offline --lib telemetry::
cargo test --locked --offline --lib reducer_graph_actions_include_auto_before_fixed_windows
```

The Rust tests exercise actual production state and tick paths, including late
history restore, upload-only spikes, manual ranges, cooldown, startup, clock
changes, and sampling gaps. Property tests vary rates, heights, gap lengths,
timing, and noise and require preserved burst grouping, a ten-minute ceiling,
monotonic framing within a period, and a stable return to 1m afterward.
