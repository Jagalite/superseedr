#!/bin/bash

# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

# cargo install tokio-console
# tokio-console
RUSTFLAGS="--cfg tokio_unstable" cargo run --features console
