# npm distribution development and release

The main `superseedr` package contains only the launcher. Five optional packages
carry native executables: `superseedr-darwin-arm64`, `superseedr-darwin-x64`,
`superseedr-linux-arm64`, `superseedr-linux-x64`, and `superseedr-win32-x64`.
The main package includes the repository README.md unchanged; platform packages
use npm/README.md for installation notes.
All versions come from Cargo.toml. No postinstall scripts or runtime downloads
are used. Linux builds target glibc (Ubuntu 22.04 runners); musl is unsupported.

Run launcher integration tests with:

```sh
node --test scripts/npm/test.mjs scripts/npm/publish.test.mjs
```

To test a current native build locally (substitute your platform):

```sh
node scripts/npm/package.mjs /tmp/superseedr-npm darwin-arm64 target/release/superseedr
node scripts/npm/package.mjs /tmp/superseedr-npm launcher
node scripts/npm/smoke.mjs /tmp/superseedr-npm
```

The smoke test packs tarballs, installs them globally into a temporary prefix
with scripts disabled and offline resolution, and invokes the installed command.
The existing Rust source is unchanged by this distribution work.

## CI and publication

The `npm launcher tests` workflow (`npm.yml`) tests packed launcher installation
on Linux, macOS, and Windows for relevant PRs and manual dispatches.

The existing release workflow (`rust.yml`) owns native packaging and npm
publication. On release tags it builds and smoke-tests all five native targets
in `npm_native`, then runs `publish_npm` after the GitHub release, alongside
`publish_crates_io`. Both npm jobs use `continue-on-error: true`, and no other
release job depends on either npm job. A failed npm deployment therefore leaves
the existing release path independent. The publisher requires every native
package build to finish before attempting publication.

The native jobs check that the tag matches Cargo.toml, test the launcher, and
verify the packaged executable's version and help output. The publisher uploads
platform packages before the main launcher. npm publication currently uses the
default `latest` dist-tag; bootstrap uploads were published separately.

Configure trusted publishing separately for all six package names:

- GitHub owner: `Jagalite`
- Repository: `superseedr`
- Workflow filename: `rust.yml`
- Environment: `npm`
- Allowed action: direct `npm publish`

The workflow must be pushed before it can be used. Configure the GitHub `npm`
environment with any desired reviewers. The publishing job alone receives
`id-token: write`; it uses Node 24's bundled npm and no stored npm token.
See https://docs.npmjs.com/trusted-publishers/ for current npm requirements.

The first publication of each package still requires npm account authentication
before its trusted publisher can be configured. npm package versions are
immutable: use a new version for changed package contents. Rerunning a partially
published release verifies each existing version's registry integrity against its
local tarball and skips exact matches. It publishes missing platform packages
before the launcher. A mismatch, registry outage, or authentication error stops
the preflight before any upload; only HTTP 404 is treated as a missing version.

The package README links users to versioned source in the repository. Retain
the matching tag/source and GPL license when distributing native artifacts.
Native installers continue to supply OS-level magnet-link registration.

Local testing alone does not qualify interactive terminal behavior, macOS
Gatekeeper/notarization, or every target's runtime compatibility. npm binaries
are built separately from the signed installer pipeline. Verify these on the
supported hosts before advertising npm installation as a stable release channel.

## Bootstrapping the existing 1.0.16 release

The packager accepts an explicit historical release version without changing
Cargo.toml or Cargo.lock:

```sh
node scripts/npm/package.mjs /tmp/release-packages darwin-arm64 /path/to/released/superseedr --version=1.0.16
node scripts/npm/package.mjs /tmp/release-packages launcher --version=1.0.16
```

Use binaries from the matching GitHub release, verify asset SHA-256 digests
against the release API metadata, and prepare all five platform packages before
the launcher. The macOS universal binary can be used in both macOS packages.
The smoke test reads the expected version from the generated launcher manifest.
Publish initial packages with `--tag bootstrap` so stable-channel promotion can
follow cross-platform validation. Configure trusted publishing separately for
all six packages; enable the direct `npm publish` allowed action used by the
workflow. Use a private credential entry or interactive `npm login` for the
first publication, and keep credentials out of source files and command output.
