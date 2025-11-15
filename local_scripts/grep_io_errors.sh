#!/bin/bash

# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

tail -f logs/app.log | grep Retrying
