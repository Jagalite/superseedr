#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later
"""Reproduce the AUTO detector selection experiment using only the standard library.

This is a fixed-step reference experiment, not the production implementation.
Both candidates share median filtering, baseline learning, burst grouping, and
framing; only the entry detector differs. Production timestamp handling, UI
switching delays, and history restoration are covered by the Rust tests.
See docs/auto-graph-policy.md for assumptions and metric definitions.
"""

import random
import statistics


class Activity:
    def __init__(self, kind):
        self.kind = kind
        self.recent = []
        self.base = self.fast = self.deviation = self.cusum = 0.0
        self.start = self.last_active = None
        self.mode = 60

    def observe(self, t, raw):
        warming_up = len(self.recent) < 3
        self.recent = (self.recent + [raw])[-3:]
        value = statistics.median(self.recent)
        if warming_up:
            self.base = self.fast = value
            return False
        self.fast += 0.35 * (value - self.fast)
        margin = max(self.base * 0.5, self.deviation * 4, 4096)
        self.cusum = max(0, self.cusum + (value - self.base) / margin - 0.5)
        detected = (self.fast > self.base + margin if self.kind == 'EWMA'
                    else self.cusum >= 4)
        if self.start is None and detected and value > self.base + margin:
            self.start = t
        active = self.start is not None and value > self.base + margin * 0.5
        if active:
            self.last_active = t
            span = t - self.start + 1
            self.mode = max(self.mode, 60 if span <= 48 else 300 if span <= 240 else 600)
        elif self.start is not None and t - self.last_active > 120:
            self.start = self.last_active = None
            self.mode = 60
            self.fast = value
            self.cusum = 0
        if self.start is None and value <= self.base + margin * 0.5:
            error = value - self.base
            self.base += 0.02 * error
            self.deviation += 0.02 * (min(abs(error), margin) - self.deviation)
        return active


def run(kind, seed, scale, jitter, amplitudes, gaps):
    rng = random.Random(seed)
    intervals = []
    start = 600
    for amp, gap in zip(amplitudes, gaps):
        intervals.append((start, start + 60, amp))
        start += 60 + gap
    detector = Activity(kind)
    delays = [None] * len(intervals)
    false = flips = 0
    previous_mode = 60
    covered = total = 0
    active_samples = []
    for t in range(start + 800):
        interval = next((i for i, (a, b, _) in enumerate(intervals) if a <= t < b), None)
        amp = intervals[interval][2] if interval is not None else 1
        value = scale * amp * (1 + rng.uniform(-jitter, jitter))
        # Single-sample upward and downward outliers outside the burst periods.
        if t == 150:
            value = scale * 100
        if t == 300:
            value = 0
        active = detector.observe(t, value)
        if interval is not None:
            if t == intervals[interval][0] and (interval == 0 or intervals[interval][0] - intervals[interval-1][1] >= 120):
                active_samples.clear()
            active_samples.append(t)
            if active and delays[interval] is None:
                delays[interval] = t - intervals[interval][0]
            visible = [s for s in active_samples if s > t - 600]
            total += len(visible)
            covered += sum(s > t - detector.mode for s in visible)
        elif active and all(t >= b + 3 or t < a for a, b, _ in intervals):
            false += 1
        if detector.mode != previous_mode:
            flips += 1
            previous_mode = detector.mode
    return delays, false, flips, covered, total


def main():
    for kind in ['EWMA', 'CUSUM']:
        delays = []
        missed = false = flips = covered = total = traces = 0
        for seed in range(10):
            for scale in [100_000, 5_000_000, 50_000_000]:
                for jitter in [0, 0.1, 0.2]:
                    for amplitudes in [[2], [20], [20, 4, 8, 2], [1.6, 2, 1.6]]:
                        for gap in [30, 90, 150]:
                            result, f, sw, cov, count = run(kind, seed, scale, jitter,
                                                            amplitudes, [gap] * len(amplitudes))
                            delays.extend(d for d in result if d is not None)
                            missed += sum(d is None for d in result)
                            false += f
                            flips += sw
                            covered += cov
                            total += count
                            traces += 1
        delays.sort()
        print(dict(detector=kind, traces=traces, detected=len(delays), missed=missed,
                   median_delay=statistics.median(delays), p95_delay=delays[int(len(delays)*.95)],
                   max_delay=max(delays), false_active_seconds=false,
                   range_changes=flips, burst_sample_coverage=covered/total), flush=True)


if __name__ == "__main__":
    main()
