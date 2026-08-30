// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    superseedr::run_native().await
}
