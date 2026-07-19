// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::lookup::LookupQualitySnapshot;
use super::persist::{PersistenceConfig, PersistenceManager};
use super::scheduler::{
    DemandEntrySnapshot, DemandFinishMode, DemandScheduler, DueDemandCandidate,
};
pub use super::scheduler::{DhtDemandMetrics, DhtDemandState};
use super::types::{AddressFamily, InfoHash, LookupId, NodeId};
use super::{AnnouncePeerJob, LookupState, Runtime, RuntimeConfig};
use crate::config::Settings;
use crate::networking::runtime::{NetworkHandle, NetworkLease};
use rand::random;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

mod api;
mod commands;
mod config;
mod driver;
mod effects;
pub(crate) mod fuzzing;
mod lifecycle;
mod monitor;
mod planner;
mod runtime;
mod state;
mod status;
mod subscribers;

#[cfg(test)]
#[path = "service/test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "service/state_tests.rs"]
mod state_tests;

#[cfg(test)]
#[path = "service/lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[path = "service/subscriber_tests.rs"]
mod subscriber_tests;

#[cfg(test)]
#[path = "service/command_tests.rs"]
mod command_tests;

#[cfg(test)]
#[path = "service/status_tests.rs"]
mod status_tests;

#[cfg(test)]
#[path = "service/monitor_tests.rs"]
mod monitor_tests;

#[cfg(test)]
#[path = "service/driver_tests.rs"]
mod driver_tests;

#[cfg(test)]
#[path = "service/runtime_effect_tests.rs"]
mod runtime_effect_tests;

#[cfg(test)]
#[path = "service/replay_tests.rs"]
mod replay_tests;

#[cfg(test)]
#[path = "service/runtime_command_replay_tests.rs"]
mod runtime_command_replay_tests;

#[cfg(test)]
#[path = "service/api_tests.rs"]
mod api_tests;

#[cfg(test)]
pub(crate) use self::api::TestDhtRecorder;
pub use self::api::{
    configured_status_from_settings, DhtDemandSubscription, DhtHandle, DhtLookupRun, DhtService,
};
pub(in crate::dht::service) use self::api::{
    send_dht_command, DhtCommand, DhtCommandReceiver, DhtCommandSender, DhtDemandSubscriptionInner,
};
use self::commands::{
    DhtRuntimeCommandAction, DhtRuntimeCommandEffect, DhtRuntimeCommandModel,
    DhtRuntimeLookupFamilyRequest,
};
pub(in crate::dht::service) use self::config::forced_internal_backend_error;
pub use self::config::{DhtBackendKind, DhtServiceConfig};
pub(in crate::dht::service) use self::driver::{command_event, run_service, LoopEvent};
use self::effects::*;
use self::lifecycle::{DhtLifecycleAction, DhtLifecycleEffect, DhtLifecycleModel};
use self::monitor::observe_action_effect_reduction;
use self::planner::*;
pub(super) use self::runtime::*;
use self::state::{
    DhtDemandCommandAction, DhtDemandCommandEffect, DhtServiceAction, DhtServiceEffect,
    DhtServiceModel, DhtServiceState,
};
pub(in crate::dht::service) use self::status::{
    build_status, build_wave_telemetry, publish_status, publish_wave_telemetry, RecentUniquePeers,
};
pub use self::status::{DhtHealthSnapshot, DhtSizeEstimate, DhtStatus, DhtWaveTelemetry};
use self::subscribers::{DemandSubscriberAction, DemandSubscriberEffect, DemandSubscriberRegistry};

const DHT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
const DHT_REBIND_TRANSPORT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const DHT_ROUTINE_LOOKUP_REFRESH_INTERVAL: Duration = DHT_MAINTENANCE_INTERVAL;
const DHT_NO_CONNECTED_PEERS_BASE_INTERVAL: Duration = Duration::from_secs(16);
const DHT_NO_CONNECTED_PEERS_MAX_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DHT_AWAITING_METADATA_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const DHT_HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const DHT_DEMAND_SCHEDULER_INTERVAL: Duration = Duration::from_millis(250);
const DHT_DEMAND_LOOKUP_SLOT_COUNT: usize = 10;
const DHT_DEMAND_LOOKUP_SLOT_FILL_PER_TICK: usize = 5;
const DHT_DRAIN_LOOKUPS_PER_VIRTUAL_SLOT: usize = 16;
const DHT_PLANNER_TOKEN_SCALE: u64 = 1_000;
const DHT_AWAITING_METADATA_LAUNCHES_PER_MINUTE: u64 = 30;
const DHT_AWAITING_METADATA_LAUNCH_BURST: u64 = 8;
const DHT_NO_CONNECTED_PEERS_LAUNCHES_PER_MINUTE: u64 = 30;
const DHT_NO_CONNECTED_PEERS_LAUNCH_BURST: u64 = 10;
const DHT_ROUTINE_REFRESH_LAUNCHES_PER_MINUTE: u64 = 5;
const DHT_ROUTINE_REFRESH_LAUNCH_BURST: u64 = 5;
const DHT_DEMAND_FAIRNESS_AGE: Duration = Duration::from_secs(10 * 60);
const DHT_DEMAND_SPARE_RESEARCH_MAX_ACTIVE: usize = 1;
const DHT_DEMAND_SPARE_RESEARCH_LAUNCH_LIMIT: usize = 1;
const DHT_DEMAND_SPARE_RESEARCH_MIN_INTERVAL: Duration = Duration::from_secs(20);
const DHT_DEMAND_USEFUL_YIELD_BOOST_MAX_AGE: Duration = Duration::from_secs(5 * 60);
const DHT_DEMAND_STRONG_YIELD_BOOST_MAX_AGE: Duration = Duration::from_secs(2 * 60);
const DHT_DEMAND_STRONG_YIELD_BOOST_MIN_UNIQUE_PEERS: usize = 64;
const DHT_DEMAND_POWER_BASE_SCALE_HALVES: u8 = 2;
const DHT_DEMAND_POWER_MAX_SCALE_HALVES: u8 = 8;
const DHT_PEER_PRESSURE_CAP_RAMP_UP_INTERVAL: Duration = Duration::from_secs(30);
const DHT_IDLE_SPEED_PROBE_2X_MIN_IDLE: Duration = Duration::from_secs(30);
const DHT_IDLE_SPEED_PROBE_3X_MIN_IDLE: Duration = Duration::from_secs(60);
const DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE: Duration = Duration::from_secs(120);
const DHT_IDLE_SPEED_PROBE_DECAY_INTERVAL: Duration = Duration::from_secs(30);
const DHT_AWAITING_METADATA_SLOT_CAP: usize = DHT_DEMAND_LOOKUP_SLOT_COUNT;
const DHT_NO_CONNECTED_PEERS_SLOT_CAP: usize = 8;
const DHT_ROUTINE_LOOKUP_SLOT_CAP: usize = 3;
const DHT_PERSISTENCE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const DHT_STARTUP_BOOTSTRAP_DELAY: Duration = Duration::from_secs(5);
const DHT_IPV6_HEDGE_DELAY: Duration = Duration::from_millis(750);
const DHT_LOOKUP_BOOTSTRAP_WAIT: Duration = Duration::from_secs(2);
const DHT_UNIQUE_PEERS_FOUND_WINDOW: Duration = Duration::from_secs(10);
const DHT_PARKED_CRAWL_MAX_AGE: Duration = Duration::from_secs(5 * 60);
const DHT_DEMAND_DRAIN_MAX_AGE: Duration = Duration::from_secs(5);
const DHT_DEMAND_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DHT_DEMAND_DRAIN_MAX_INFLIGHT_QUERIES: usize = 128;
const DHT_DEMAND_DRAIN_NO_LATE_YIELD_GRACE: Duration = Duration::from_millis(1500);
const DHT_AWAITING_METADATA_DRAIN_NO_LATE_YIELD_GRACE: Duration = Duration::from_secs(2);
const DHT_ROUTINE_DRAIN_NO_LATE_YIELD_GRACE: Duration = Duration::from_millis(750);
const DHT_AWAITING_METADATA_SLICE_WALL_TIME: Duration = Duration::from_secs(6);
const DHT_AWAITING_METADATA_SLICE_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const DHT_NO_CONNECTED_PEERS_SLICE_WALL_TIME: Duration = Duration::from_secs(4);
const DHT_NO_CONNECTED_PEERS_SLICE_IDLE_TIMEOUT: Duration = Duration::from_millis(1500);
const DHT_ROUTINE_SLICE_WALL_TIME: Duration = Duration::from_secs(2);
const DHT_ROUTINE_SLICE_IDLE_TIMEOUT: Duration = Duration::from_millis(750);
const DHT_ROUTINE_SUPPORT_SLICE_WALL_TIME: Duration = Duration::from_secs(4);
const DHT_ROUTINE_SUPPORT_SLICE_IDLE_TIMEOUT: Duration = Duration::from_millis(1500);
const DHT_AWAITING_METADATA_SLICE_UNIQUE_PEER_CAP: usize = 128;
const DHT_NO_CONNECTED_PEERS_SLICE_UNIQUE_PEER_CAP: usize = 48;
const DHT_ROUTINE_SLICE_UNIQUE_PEER_CAP: usize = 16;
const DHT_ROUTINE_SUPPORT_SLICE_UNIQUE_PEER_CAP: usize = 48;
const DHT_AWAITING_METADATA_STALLED_EMPTY_SLICE_RESET_THRESHOLD: u32 = 4;
const DHT_NO_CONNECTED_PEERS_STALLED_EMPTY_SLICE_RESET_THRESHOLD: u32 = 3;
const DHT_ROUTINE_STALLED_EMPTY_SLICE_RESET_THRESHOLD: u32 = 2;
const DHT_AWAITING_METADATA_STALLED_LOW_YIELD_SLICE_MAX_UNIQUE_PEERS: usize = 0;
const DHT_NO_CONNECTED_PEERS_STALLED_LOW_YIELD_SLICE_MAX_UNIQUE_PEERS: usize = 2;
const DHT_ROUTINE_STALLED_LOW_YIELD_SLICE_MAX_UNIQUE_PEERS: usize = 1;
const DHT_NO_CONNECTED_PEERS_WEAK_PARKED_MIN_VISITED: usize = 12;
const DHT_NO_CONNECTED_PEERS_WEAK_PARKED_MAX_RESPONDERS: usize = 3;
const DHT_NO_CONNECTED_PEERS_WEAK_PARKED_MAX_FRONTIER: usize = 8;
const DHT_NO_CONNECTED_PEERS_WEAK_PARKED_MAX_RECEIVED_PEERS: usize = 12;
const DHT_ROUTINE_WEAK_PARKED_MIN_VISITED: usize = 8;
const DHT_ROUTINE_WEAK_PARKED_MAX_RESPONDERS: usize = 1;
const DHT_ROUTINE_WEAK_PARKED_MAX_FRONTIER: usize = 4;
const DHT_ROUTINE_WEAK_PARKED_MAX_RECEIVED_PEERS: usize = 4;
