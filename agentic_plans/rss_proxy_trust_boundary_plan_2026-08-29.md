# Secure RSS Proxy Trust-Boundary Implementation Plan

Date: 2026-08-29

## Status

- Planning only.
- The v1.0.14 release keeps RSS requests direct, disables automatic proxies, and
  retains public-destination DNS and redirect filtering in every network mode.
- Do not re-enable RSS proxy routing until the trust model, configuration boundary,
  and validation matrix in this plan are implemented.

## Agent Execution Handoff

Use this section when starting or resuming implementation.

### Objective

Add optional RSS proxy support without weakening the secure direct-request path or
silently changing the meaning of the `public_only` policy.

The implementation must distinguish two explicit trust models:

1. `DirectPublicOnly`: Superseedr resolves and connects to public RSS destinations
   directly and rejects local, private, unroutable, credentialed, and unsafe redirect
   targets.
2. `ExplicitTrustedProxy`: the user explicitly authorizes a named proxy to resolve
   and reach RSS destinations. Superseedr still rejects unsafe URL syntax and private
   literals, but the proxy becomes the DNS and destination-policy trust boundary.

### Constraints

- Keep `DirectPublicOnly` as the default for new and existing configurations.
- Do not automatically trust `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, operating-
  system proxy settings, or inherited `NO_PROXY` behavior.
- Do not describe proxied RSS requests as `public_only`; the application cannot
  prove the proxy's remote DNS answer or reachable network.
- Keep Interface and Local Address binding fail-closed and proxy-free in the first
  implementation.
- Never fall back from a failed trusted-proxy request to a direct request, or from a
  failed strict/direct request to a proxy.
- Do not put proxy credentials in shared configuration, status JSON, event-journal
  details, errors, or logs.
- Preserve existing feed configuration and RSS history formats unless a deliberate,
  backward-compatible schema addition is required.
- Keep proxy policy host-scoped; it must not be shared as torrent catalog state.
- Update this plan as decisions are made and record validation evidence before
  marking any phase complete.

### Completion Criteria

Treat the work as complete only when:

- default RSS traffic still uses `.no_proxy()` plus `PublicFilteringResolver`;
- trusted proxy use requires an explicit host-scoped opt-in;
- direct and proxied requests use separate clients and separately named policies;
- the UI, CLI, status output, and documentation state which component owns DNS and
  destination trust;
- private named proxy endpoints work only in explicit trusted-proxy mode;
- direct requests, including `NO_PROXY`-like exceptions, cannot accidentally use the
  proxy client's permissive resolver;
- strict binding modes reject proxy enablement with an actionable message;
- credentials are redacted and covered by tests;
- the full security, compatibility, and platform matrix passes.

## Problem Statement

Reqwest's automatic proxy selection and a single custom DNS resolver cannot enforce
both desired properties:

- A `PublicFilteringResolver` protects direct RSS requests from DNS rebinding and
  private-address resolution, but it also resolves the proxy endpoint and therefore
  rejects common private or loopback proxy hosts.
- A family-only resolver permits private proxy endpoints, but then direct RSS
  hostnames can resolve to private services.
- When an HTTP proxy resolves the feed hostname, the client validates only the proxy
  connection endpoint. The client cannot verify the proxy's remote DNS result or
  final reachable address.
- Automatic redirects compound the problem because a proxy may resolve each redirect
  target independently.

Therefore proxy support cannot be represented as a small change to one RSS reqwest
builder. It requires a separate, explicit trust boundary.

## Security Contract

### DirectPublicOnly

- Allow only HTTP and HTTPS feed URLs.
- Reject embedded URL credentials.
- Reject localhost and non-public IP literals before request construction.
- Resolve every direct hostname through `PublicFilteringResolver`.
- Apply the same checks to every redirect target.
- Disable all automatic and environment-derived proxies.
- Enforce enabled IPv4 and IPv6 families.
- Preserve generation invalidation and request cancellation.

### ExplicitTrustedProxy

- Require an explicit user opt-in and proxy endpoint.
- Treat the proxy as trusted to resolve and reach feed and redirect destinations.
- Continue rejecting URL credentials, localhost names, and private IP literals in
  feed URLs before handing them to the proxy.
- Resolve and connect to the proxy endpoint through a proxy-specific resolver that
  obeys the enabled address families.
- Do not claim DNS-rebinding or private-destination prevention beyond the proxy.
- Do not use direct fallback.
- Restrict the first implementation to `NetworkBindingMode::Any`.

## Non-Goals For The First Implementation

- Transparent compatibility with arbitrary inherited system proxy settings.
- Proxy use in Interface or Local Address binding modes.
- Proving the proxy's remote DNS answers are public.
- Supporting per-feed proxy selection.
- Implementing a general application-wide proxy subsystem.
- Storing plaintext proxy credentials in normal or shared TOML.
- Supporting SOCKS until its local-versus-remote DNS behavior has a separate review.

## Recommended Architecture

### Configuration

Add a host-scoped RSS network policy with an explicit default:

```text
rss_proxy_mode = "disabled"
rss_proxy_url = null
```

Prefer a typed enum internally, for example:

```text
RssProxyMode::Disabled
RssProxyMode::ExplicitTrusted
```

Do not infer `ExplicitTrusted` from environment variables. If environment-only
configuration is later required, use dedicated Superseedr variables and require the
mode variable as explicit consent.

### Client topology

Keep two distinct construction paths:

1. Direct RSS client:
   - `.no_proxy()`;
   - `PublicFilteringResolver`;
   - public-only redirect policy;
   - `NetworkHttpClient` policy named `DirectPublicOnly`.
2. Trusted proxy RSS client:
   - explicit `reqwest::Proxy`, not automatic system proxy discovery;
   - proxy-endpoint resolver that allows private addresses in `Any` mode;
   - no direct fallback and no inherited `NO_PROXY` exceptions;
   - `NetworkHttpClient` policy named `ExplicitTrustedProxy`.

Do not hide both clients behind a boolean named `public_only`. Make the trust policy
part of the type or request state so call sites, errors, and tests cannot confuse the
two guarantees.

### Credentials

- Prefer an operating-system credential facility or a dedicated environment variable
  over persisted plaintext.
- Redact user info before formatting proxy URLs.
- Ensure builder errors and connection errors cannot expose credentials.
- Never emit proxy authorization headers into diagnostics or test artifacts.

### User experience

- Label the option `Trusted RSS proxy`, not `Use system proxy`.
- Explain that the proxy controls DNS and may reach private destinations.
- Reject enablement outside `Any` mode in the first release.
- Show active policy and sanitized proxy host in diagnostics without credentials.
- Require confirmation when enabling the trusted-proxy mode.

## Implementation Phases

### Phase 1: Policy types and configuration

1. Add the typed RSS proxy mode to host-scoped settings.
2. Default missing fields to disabled for upgrade compatibility.
3. Add serialization, environment-override, and shared-config ownership tests.
4. Define credential sourcing and redaction before accepting proxy URLs.

Exit criteria:

- Old configurations load with secure direct behavior.
- Rollback readers ignore the new fields.
- Shared catalog files never receive host proxy settings or secrets.

### Phase 2: Separate client construction

1. Rename the current RSS policy to make `DirectPublicOnly` explicit.
2. Preserve the existing direct client without behavior changes.
3. Add an explicit trusted-proxy client builder for `Any` mode.
4. Resolve proxy endpoints independently from RSS targets.
5. Disable automatic proxies and direct fallback on both builders.
6. Keep each client generation-owned and invalidation-aware.

Exit criteria:

- The direct client cannot connect to private DNS answers.
- A named private proxy works only when explicitly trusted.
- Proxy failure does not produce a direct request.
- Strict modes cannot construct or obtain the proxy client.

### Phase 3: RSS service integration and diagnostics

1. Select the client from the typed policy when acquiring the generation lease.
2. Return actionable, redacted errors for invalid or unavailable proxies.
3. Add status and event-journal policy visibility without secrets.
4. Keep sync retry behavior bounded and generation-aware.

Exit criteria:

- A policy change invalidates the old client generation before new RSS work starts.
- Logs identify direct versus trusted-proxy policy without printing credentials.
- RSS sync remains deferred while the selected network policy is unavailable.

### Phase 4: UI, CLI, and documentation

1. Add host-scoped configuration controls with confirmation text.
2. Add CLI inspection and mutation support.
3. Document the trust boundary, supported modes, and migration behavior.
4. Add a release note only when proxy support is actually shipped.

Exit criteria:

- Users cannot enable trusted proxy mode without seeing the DNS/destination warning.
- Help and documentation do not imply equivalent security between direct and proxied
  requests.

## Validation Matrix

### Direct security

- Public IPv4 and IPv6 feed destinations succeed.
- Loopback, link-local, private, multicast, unspecified, and mapped literals fail.
- Public hostnames resolving to private addresses fail before connecting.
- Redirects to private literals or private DNS answers fail.
- Proxy environment variables do not affect the direct client.
- DNS rebinding and mixed public/private answer sets fail closed.

### Trusted proxy behavior

- A proxy addressed by public DNS succeeds.
- A proxy addressed by private DNS succeeds only after explicit opt-in.
- HTTP and HTTPS feed requests use the proxy and never create a direct connection.
- Proxy authentication succeeds without credential disclosure.
- Proxy startup, DNS, authentication, and connection failures do not fall back.
- Disabling the policy immediately restores direct public-only behavior.

### Binding and lifecycle

- Interface and Local Address modes reject trusted proxy enablement.
- `Any` preserves normal torrent networking regardless of RSS proxy policy.
- Generation invalidation cancels in-flight direct and proxied RSS requests.
- IPv4-only and IPv6-only policy applies to the proxy endpoint.
- Status snapshots from older versions deserialize with proxy mode disabled.

### Regression gates

- Formatting and strict Clippy for all targets and features.
- Full all-features and no-default-features test suites.
- Linux, macOS, and Windows compilation.
- RSS service lifecycle and retry tests.
- Production-network-construction inventory remains centralized.
- A proxy integration fixture verifies request routing without real services or
  credentials.

## Primary Files Expected To Change

- `src/config.rs`
- `src/networking/runtime.rs`
- `src/networking/dns.rs`
- `src/integrations/rss_service.rs`
- `src/integrations/status.rs`
- `src/tui/screens/config.rs`
- `src/command.rs`
- `docs/configuration-and-backups.md`
- `docs/native-network-binding.md`
- `docs/CHANGELOG.md` only when implementation ships

## Open Decisions

1. Whether proxy credentials use only environment variables or an operating-system
   credential facility.
2. Whether HTTPS proxies are required in the first implementation.
3. Whether proxy certificate trust needs a separate configuration surface.
4. Whether the trusted-proxy policy belongs under RSS settings or a future host HTTP
   transport settings group.
5. Whether a future strict-binding proxy mode can be qualified without weakening the
   existing no-proxy guarantee.

## Release Decision

For v1.0.14, retain the secure direct RSS client and do not advertise proxy
compatibility. This plan is a follow-up design and implementation boundary, not a
shipped capability.
