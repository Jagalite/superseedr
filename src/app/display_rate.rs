// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application display rate definitions and transitions.

use super::*;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DataRate {
    RateQuarter,
    RateHalf,
    #[default]
    Rate1s,
    Rate2s,
    Rate4s,
    Rate10s,
    Rate20s,
    Rate30s,
    Rate60s,
}

impl DataRate {
    /// Returns the millisecond value for the data rate.
    pub fn as_ms(&self) -> u64 {
        match self {
            DataRate::RateQuarter => 4000,
            DataRate::RateHalf => 2000,
            DataRate::Rate1s => 1000,
            DataRate::Rate2s => 500,
            DataRate::Rate4s => 250,
            DataRate::Rate10s => 100,
            DataRate::Rate20s => 50,
            DataRate::Rate30s => 33,
            DataRate::Rate60s => 17,
        }
    }

    pub fn fps_label(self) -> &'static str {
        match self {
            DataRate::RateQuarter => "0.25",
            DataRate::RateHalf => "0.5",
            DataRate::Rate1s => "1",
            DataRate::Rate2s => "2",
            DataRate::Rate4s => "4",
            DataRate::Rate10s => "10",
            DataRate::Rate20s => "20",
            DataRate::Rate30s => "30",
            DataRate::Rate60s => "60",
        }
    }

    pub fn target_fps(self) -> f64 {
        match self {
            DataRate::RateQuarter => 0.25,
            DataRate::RateHalf => 0.5,
            DataRate::Rate1s => 1.0,
            DataRate::Rate2s => 2.0,
            DataRate::Rate4s => 4.0,
            DataRate::Rate10s => 10.0,
            DataRate::Rate20s => 20.0,
            DataRate::Rate30s => 30.0,
            DataRate::Rate60s => 60.0,
        }
    }

    pub fn frame_interval(self) -> Duration {
        Duration::from_secs_f64(1.0 / self.target_fps())
    }

    /// Cycles to the next (slower) data rate (lower FPS).
    pub fn next_slower(&self) -> Self {
        match self {
            DataRate::Rate60s => DataRate::Rate30s,
            DataRate::Rate30s => DataRate::Rate20s,
            DataRate::Rate20s => DataRate::Rate10s,
            DataRate::Rate10s => DataRate::Rate4s,
            DataRate::Rate4s => DataRate::Rate2s,
            DataRate::Rate2s => DataRate::Rate1s,
            DataRate::Rate1s => DataRate::RateHalf,
            DataRate::RateHalf => DataRate::RateQuarter,
            DataRate::RateQuarter => DataRate::RateQuarter,
        }
    }

    /// Cycles to the previous (faster) data rate (higher FPS).
    pub fn next_faster(&self) -> Self {
        match self {
            DataRate::RateQuarter => DataRate::RateHalf,
            DataRate::RateHalf => DataRate::Rate1s,
            DataRate::Rate1s => DataRate::Rate2s,
            DataRate::Rate2s => DataRate::Rate4s,
            DataRate::Rate4s => DataRate::Rate10s,
            DataRate::Rate10s => DataRate::Rate20s,
            DataRate::Rate20s => DataRate::Rate30s,
            DataRate::Rate30s => DataRate::Rate60s,
            DataRate::Rate60s => DataRate::Rate60s,
        }
    }
}
