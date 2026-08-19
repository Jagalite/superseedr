# Remote Agentic Development

This document describes how to create a disposable remote development
environment for Superseedr using an EC2 instance.

The goal is to make cloud development machines disposable:

1. Launch instance.
2. Run bootstrap script.
3. Authenticate GitHub and Codex.
4. Create worktrees and run agents.
5. Push all valuable work to GitHub.
6. Audit worktrees.
7. Terminate the instance and its EBS volume.

No credentials or important source code should depend on the continued
existence of the EC2 instance.

---

## Recommended EC2 Configuration

The initial tested configuration is:

| Setting | Value |
|---|---|
| OS | Debian 13 |
| Architecture | x86_64 / AMD64 |
| Purchase option | On-Demand |
| Instance type | `m8a.xlarge` |
| vCPU | 4 |
| RAM | 16 GiB |
| Root storage | 100 GiB gp3 |
| Public IPv4 | Enabled |
| SSH | Public-key authentication only |

The `m8a.xlarge` is sufficient for several concurrent Codex agents, although
multiple simultaneous Rust builds can saturate its four CPUs.

If CPU contention becomes significant, test an 8-vCPU compute-optimized
instance before increasing memory.

---

## Security Group

SSH should not be exposed globally.

Allow:

```text
TCP 22
Source: <your-public-IP>/32
