#!/usr/bin/env bash

# Superseedr disposable development/build box bootstrap
#
# Target:
#   Debian 13 x86_64
#
# Installs:
#   - Base development tools
#   - GitHub CLI
#   - Rust 1.95.0 + rustfmt + clippy
#   - Python integration-test environment
#   - Codex CLI
#   - Monitoring tools
#   - Superseedr repository
#
# Does NOT:
#   - install Docker
#   - authenticate GitHub
#   - authenticate Codex
#   - install AWS credentials
#   - modify SSH configuration
#
# Safe to rerun.

set -euo pipefail

REPO_URL="https://github.com/Jagalite/superseedr.git"
REPO_DIR="$HOME/superseedr"
RUST_VERSION="1.95.0"
NPM_PREFIX="$HOME/.local/npm"

log() {
    printf '\n\033[1;34m==> %s\033[0m\n' "$1"
}

log "Updating Debian package metadata"

sudo apt-get update

log "Installing development packages"

sudo apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    git \
    curl \
    ca-certificates \
    tmux \
    btop \
    htop \
    sysstat \
    hyperfine \
    ripgrep \
    jq \
    python3 \
    python3-pip \
    python3-venv \
    nodejs \
    npm

log "Enabling sysstat telemetry"

sudo systemctl enable --now sysstat || true

log "Installing GitHub CLI repository"

sudo mkdir -p -m 755 /etc/apt/keyrings

if [ ! -f /etc/apt/keyrings/githubcli-archive-keyring.gpg ]; then
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        | sudo tee /etc/apt/keyrings/githubcli-archive-keyring.gpg >/dev/null

    sudo chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg
fi

if [ ! -f /etc/apt/sources.list.d/github-cli.list ]; then
    echo \
        "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        | sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null
fi

sudo apt-get update
sudo apt-get install -y gh

log "Installing Rust"

if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y
fi

# shellcheck disable=SC1091
source "$HOME/.cargo/env"

log "Installing Rust $RUST_VERSION"

rustup toolchain install "$RUST_VERSION" \
    --component rustfmt \
    --component clippy

rustup default "$RUST_VERSION"

log "Configuring user-local npm installation directory"

mkdir -p "$NPM_PREFIX"

npm config set prefix "$NPM_PREFIX"

if ! grep -Fq '.local/npm/bin' "$HOME/.bashrc" 2>/dev/null; then
    echo 'export PATH="$HOME/.local/npm/bin:$PATH"' >> "$HOME/.bashrc"
fi

export PATH="$NPM_PREFIX/bin:$PATH"

log "Installing Codex CLI"

npm install -g @openai/codex

log "Cloning or updating Superseedr"

if [ -d "$REPO_DIR/.git" ]; then
    cd "$REPO_DIR"

    if [ -z "$(git status --porcelain)" ]; then
        git pull --ff-only
    else
        echo "Repository contains local changes; skipping git pull."
    fi
else
    git clone "$REPO_URL" "$REPO_DIR"
    cd "$REPO_DIR"
fi

log "Creating Python integration-test environment"

if [ ! -d "$REPO_DIR/.venv" ]; then
    python3 -m venv "$REPO_DIR/.venv"
fi

# shellcheck disable=SC1091
source "$REPO_DIR/.venv/bin/activate"

python -m pip install --upgrade pip

if [ -f "$REPO_DIR/requirements-integration.txt" ]; then
    python -m pip install -r "$REPO_DIR/requirements-integration.txt"
fi

log "Restricting credential directories if they already exist"

[ -d "$HOME/.config/gh" ] && chmod -R go-rwx "$HOME/.config/gh" || true
[ -d "$HOME/.codex" ] && chmod -R go-rwx "$HOME/.codex" || true

log "Environment summary"

echo
echo "Host:"
uname -a

echo
echo "CPU:"
printf 'vCPUs: '
nproc

echo
echo "Memory:"
free -h

echo
echo "Rust:"
rustc --version
cargo --version

echo
echo "Node:"
node --version
npm --version

echo
echo "GitHub CLI:"
gh --version | head -1

echo
echo "Codex:"
codex --version

echo
echo "Repository:"
cd "$REPO_DIR"
git status --short --branch

cat <<'EOF'

============================================================
Superseedr development box setup complete.
============================================================

Manual steps remaining:

1. Authenticate GitHub:

   gh auth login

2. Authenticate Codex:

   codex login

3. Start a persistent Codex session:

   cd ~/superseedr
   source .venv/bin/activate
   tmux new -s codex
   codex

Useful monitoring:

   btop
   sar -u
   sar -q
   sar -r

EOF
