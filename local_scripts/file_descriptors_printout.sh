#!/bin/bash

# SPDX-FileCopyrightText: 2025 Jaga Tranvo 
# SPDX-License-Identifier: GPL-3.0-or-later

# ==============================================================================
# SCRIPT CONFIGURATION
# ==============================================================================
# 1. Replace 'your_app_name' with your executable name.
APP_NAME="superseedr" 
# 2. Replace '37345' with your application's listening port.
CLIENT_PORT="37345" 
# ==============================================================================

# --- OS Check for dependencies ---
if ! command -v lsof &> /dev/null || ! command -v pgrep &> /dev/null || ! command -v netstat &> /dev/null; then
    echo "Error: This script requires lsof, pgrep, and netstat (standard on macOS)."
    exit 1
fi

# ==============================================================================
# 1. FIND ALL RELATED PIDS
# ==============================================================================

# Find ALL PIDs associated with the application name.
# Output format is a comma-separated list suitable for 'lsof -p'
ALL_PIDS=$(pgrep "$APP_NAME" | tr '\n' ',' | sed 's/,$//')

if [ -z "$ALL_PIDS" ]; then
    echo "Error: Could not find any running processes with name '$APP_NAME'."
    exit 1
fi

echo "--- Comprehensive FD Monitor for PIDs ($ALL_PIDS) (macOS) ---"

# ==============================================================================
# 2. RETRIEVE LIMITS (Using the limit of the primary/first PID)
# ==============================================================================

# Get the primary PID for ulimit check (ulimit is shell/process-specific)
PRIMARY_PID=$(pgrep "$APP_NAME" | head -n 1)

# Note: ulimit -n command must be run in the shell's context, not against the process.
# We trust that the primary process's limits are representative.
SOFT_LIMIT=$(ulimit -Sn)
HARD_LIMIT=$(ulimit -Hn)


# ==============================================================================
# 3. MEASUREMENT FUNCTIONS
# ==============================================================================

# Method A: Direct Process Inspection (Now includes ALL PIDs)
function get_lsof_process_count() {
    # Run lsof against ALL PIDs and count the total unique FDs.
    # -a ensures all criteria are met (i.e., open file AND specific PIDs)
    # -P and -n for performance
    # grep -v PID excludes the header line.
    
    LSOF_LINES=$(sudo lsof -a -P -n -p "$ALL_PIDS" 2>/dev/null | grep -v 'PID' | wc -l)
    
    # Check if we got a count or if the processes terminated
    if [ "$LSOF_LINES" -gt 0 ]; then
        echo "$LSOF_LINES"
    else
        echo 0
    fi
}

# Method B: Network Connection Count (No Change: netstat captures all sockets regardless of PID)
function get_network_socket_count() {
    # Count established connections on the client's listening port
    sudo netstat -an 2>/dev/null | grep "ESTABLISHED" | grep ":$CLIENT_PORT" | wc -l
}

# Method C: Combined Estimated Total (Best diagnostic value)
function estimate_total_fds() {
    local SOCKETS=$(get_network_socket_count)
    
    # Estimate non-socket FDs (Disk handles, logger, channels, etc.)
    # We use a conservative estimate for the fixed file handle pool and system overhead.
    local FILE_AND_SYSTEM_OVERHEAD=30 
    
    echo $((SOCKETS + FILE_AND_SYSTEM_OVERHEAD))
}


# ==============================================================================
# 4. DISPLAY RESULTS
# ==============================================================================

LSOF_COUNT=$(get_lsof_process_count)
SOCKETS_COUNT=$(get_network_socket_count)
ESTIMATED_TOTAL=$(estimate_total_fds)

echo ""
echo "========================================================="
echo "  FILE DESCRIPTOR LIMITS"
echo "========================================================="
echo "  Soft Limit (Crash Point):      $SOFT_LIMIT"
echo "  Hard Limit (Max Possible):     $HARD_LIMIT"
echo "---------------------------------------------------------"
echo "  CURRENT FD USAGE"
echo "---------------------------------------------------------"
echo "  M1: Direct LSOF Count (All PIDs): $LSOF_COUNT"
echo "       (This should be the most accurate total FD count)"
echo ""
echo "  M2: Active Socket Count:       $SOCKETS_COUNT"
echo "       (Reliable count of your peer connections)"
echo ""
echo "  M3: ESTIMATED TRUE USAGE:      $ESTIMATED_TOTAL"
echo "       (M2 + conservative estimate for files/system)"
echo "========================================================="

# ==============================================================================
# 5. DIAGNOSIS
# ==============================================================================

if [ "$SOFT_LIMIT" -gt 0 ]; then
    # Use the M1 count (LSOF_COUNT) for the most accurate diagnosis.
    if [ "$LSOF_COUNT" -ge "$SOFT_LIMIT" ]; then
        echo ""
        echo "!!! CRITICAL DIAGNOSIS !!!"
        echo "!!! Observed FDs ($LSOF_COUNT) EXCEEDS the Soft Limit ($SOFT_LIMIT). !!!"
        echo "!!! Action: You MUST raise 'ulimit -n' to fix the crash. !!!"
        echo ""
    elif [ "$LSOF_COUNT" -ge $((SOFT_LIMIT * 80 / 100)) ]; then
        echo ""
        echo "!!! WARNING: FD usage is above 80% of the Soft Limit. !!!"
        echo "!!! Spikes will cause the 'Too many open files' error. !!!"
        echo ""
    else
        echo ""
        echo "Diagnosis: Usage is currently safe. The low limit ($SOFT_LIMIT) is still the root cause."
    fi
fi
