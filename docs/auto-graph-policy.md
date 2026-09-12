# AUTO graph: live activity and idle history

AUTO follows meaningful rises in combined download and upload throughput. It
keeps related spikes together within a rolling ten-minute maximum while traffic
is flowing. After two minutes of zero download and upload throughput, AUTO can
use saved history to choose a longer range, through 1y.

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
8. After the burst cooldown, the live target returns to 1m. Expiring history,
   restored data, or old peaks cannot create a new burst because none of them
   are detector inputs.

## Idle history policy

Track zero throughput separately from burst detection. Steady traffic, including
rates below the burst threshold, is still activity and keeps AUTO within the
live 1m/5m/10m ranges. Every nonzero sample resets the idle timer. Startup requires
two minutes of live zero samples before history becomes eligible; restoring a
history file does not advance this timer.

While idle, score the retained history for each fixed range using its activity
span, lead-in, variation, transitions, recency, and whether the start is clipped.
Each candidate must have enough retained history for the preceding range and
meaningful nonzero traffic in its own window. Ties favor the shorter range. An
empty or all-zero history stays at 1m instead of widening to a blank view.

The existing display controller reevaluates every five seconds and widens at
most one marker per twenty seconds, including when exploring idle history.
Any resumed download or upload leaves a range above 10m on that same telemetry
tick, bypassing the evaluation interval and returning directly to the live
target. Detector updates continue while a manual range is selected, so returning
to AUTO can frame an already detected burst. Manual ranges through 1y remain
available.

Duplicate timestamps are ignored by the detector. Clock rollback or a sampling
interruption longer than two minutes starts fresh calibration and resets the
idle timer. Shorter sampling gaps retain the last three real observations.
Signal and baseline smoothing use
elapsed-time weights (`1 - (1 - alpha)^seconds`) so repeated delayed ticks still
detect sustained rises. The median still needs real samples; missing seconds
are not filled with copies of the next observation.
The normal native and browser second-tick paths share this implementation.
