// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Destination and shared-role decisions after host-specific input acquisition.

use super::*;

pub(crate) enum AddDestination {
    Manual,
    Direct(PathBuf),
    Forward(PathBuf),
    Reject(String),
}

pub(crate) fn choose_add_destination(
    settings: &Settings,
    follower: bool,
    host_watch_input: bool,
    shared_inbox_input: bool,
    leader_runtime: bool,
) -> AddDestination {
    let missing_destination = || {
        AddDestination::Reject(
        "Follower add ingest requires a default download folder so the leader can apply the torrent without local manual UI.".into())
    };
    if follower && !shared_inbox_input {
        return settings
            .default_download_folder
            .clone()
            .map(AddDestination::Forward)
            .unwrap_or_else(missing_destination);
    }
    if settings.always_show_add_location_prompt
        && !host_watch_input
        && (!shared_inbox_input || leader_runtime)
    {
        return AddDestination::Manual;
    }
    if let Some(path) = &settings.default_download_folder {
        AddDestination::Direct(path.clone())
    } else if follower {
        missing_destination()
    } else {
        AddDestination::Manual
    }
}

pub(super) fn capabilities_for_cluster_role(
    shared: bool,
    role: Option<AppClusterRole>,
    host: AppCapabilities,
) -> ClusterCapabilities {
    let follower = shared && role == Some(AppClusterRole::Follower);
    ClusterCapabilities {
        can_write_shared_state: !follower,
        can_queue_shared_commands: host.shared_cluster && shared,
        can_edit_host_local_config: !shared || follower,
        can_persist_local_runtime_state: !follower,
        can_consume_shared_inbox: !shared || role == Some(AppClusterRole::Leader),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interactive_prompt_does_not_change_watch_ingest_or_follower_routing() {
        let settings = Settings {
            always_show_add_location_prompt: true,
            default_download_folder: Some(PathBuf::from("/fixture-downloads")),
            ..Default::default()
        };
        assert!(matches!(
            choose_add_destination(&settings, false, false, false, false),
            AddDestination::Manual
        ));
        assert!(matches!(
            choose_add_destination(&settings, false, true, false, false),
            AddDestination::Direct(_)
        ));
        assert!(matches!(
            choose_add_destination(&settings, true, false, false, false),
            AddDestination::Forward(_)
        ));
    }
}
