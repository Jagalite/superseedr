# Unpaused memory experiments

Saved prototype, not production behavior. Base: 75ff6c80.

Enable `memory-profile` and select `SUPERSEEDR_MEMORY_EXPERIMENTS=utp,compact` (or either name individually). The uTP prototype imposes a 30-second deadline after full application-stream drop; that deadline can discard queued data and must not be shipped unchanged.

Two 600-second runs used private copies of the 126-torrent catalog and kept every torrent unpaused. Original configuration and payloads were protected from writes. Final 150-second requested Rust heap medians were 106.36 MiB baseline and 74.29 MiB combined (32.08 MiB observed difference). Live demand and peer histories differed, so this is not an isolated causal estimate or an RSS guarantee.

Eight controlled TCP/uTP download, upload, connected-idle and churn cases passed. Stable connected-idle allocation was essentially unchanged. Runs were rate capped and do not qualify peak throughput. Raw transfer-rate counters are bits/second; corrected summaries convert to bytes/second.

Aggregate conclusions are stored alongside this note. Full private evidence and scripts remain in `tmp/memory-126/` in the experiment worktree, excluded from Git because they contain local catalog and runtime details.
