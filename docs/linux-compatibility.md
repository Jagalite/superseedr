# Linux release compatibility

Linux releases provide GNU/Linux Debian packages and tarballs, plus separate
static musl tarballs. Both normal and private builds are available for amd64 and
arm64. Private builds disable the default peer discovery and WebTorrent features.

## GNU build baseline and Debian dependencies

GNU binaries target glibc **2.17** using the current Rust compiler and an explicit
Zig glibc target. This matches Rust's documented minimum for these GNU targets.
They dynamically use the installed system libraries: a newer
system still runs its newer glibc. The baseline is an ABI ceiling for our build,
not a bundled old libc and not a promise that every old distribution or kernel
supports the application.

```sh
rustup target add x86_64-unknown-linux-gnu
cargo zigbuild --locked --release --target x86_64-unknown-linux-gnu.2.17
python3 scripts/linux_release.py verify-glibc \
  target/x86_64-unknown-linux-gnu/release/superseedr \
  --max-glibc 2.17 --architecture amd64
```

Use `aarch64-unknown-linux-gnu.2.17` and `--architecture arm64` for ARM64.
The verifier rejects the wrong architecture, missing/private glibc version
information, and any version requirement above the baseline.

The Debian packager takes that exact prebuilt executable, rather than rebuilding
against the CI runner's libc. `scripts/linux_release.py fix-deb` checks the
packaged executable against it and runs `dpkg-shlibdeps` to populate `Depends`,
including a versioned `libc6` requirement and any other shared-library packages.
Existing manually declared dependencies are preserved. Missing library metadata
fails packaging rather than producing a dependency-free package.
The package also declares `ca-certificates`: the HTTP client requires a system
trust store even though the TLS implementation is compiled into the executable.

Dependency generation uses Ubuntu 20.04 package metadata, before the glibc 2.34
library merge, to avoid imposing newer package floors from the build host.
Debian package availability is distribution-specific: the GNU tarball's libc
baseline does not guarantee that an old distribution provides every package
named in `Depends`.

## Release validation

The reusable Linux workflow runs for PRs and ordinary builds as well as releases.
For both architectures and feature variants it checks final ELF linkage, builds
library tests in Cargo's test profile with the same target, and runs them in a
glibc 2.17 manylinux2014 container or an Alpine musl container. The GNU container's actual libc version
is asserted so an image change cannot silently raise the test baseline. It also
runs the same binaries and library tests on Ubuntu 20.04 (glibc 2.31), Ubuntu
22.04 (glibc 2.35), and the current Ubuntu 24.04 runner (glibc 2.39), and installs
GNU packages on all three Ubuntu releases. Release publication depends on these
jobs succeeding.

These checks cover linking, unit tests, and installation. They do not establish
old-kernel support, live-swarm throughput, or equal performance between libc
implementations. The localhost resolver test exercises libc host lookup without
requiring external DNS; representative tracker resolution still needs separate
validation.

## Static musl downloads

The `linux-amd64-musl` and `linux-arm64-musl` tarballs bundle the C runtime into the
executable. They do not require the host's glibc or a musl dynamic loader. They
still need the correct CPU architecture and a compatible Linux kernel, plus
ordinary runtime configuration such as DNS and trusted TLS certificates. Static
linking does not make the executable self-contained with respect to all OS data.

Release builds use Rust 1.95.0, cargo-zigbuild 0.23.4, and Zig 0.15.2. To reproduce
one after installing those tools:

```sh
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --locked --release --target x86_64-unknown-linux-musl
scripts/verify-static-musl.sh target/x86_64-unknown-linux-musl/release/superseedr
```

Use `aarch64-unknown-linux-musl` for arm64. Add `--no-default-features` for a
private build. Native C crypto dependencies are compiled through the Zig toolchain
alongside Rust dependencies; no application feature is removed from normal musl
builds to bypass them.

The static verification checks the final executable, failing if it contains an
ELF interpreter, a shared-library dependency, or a glibc symbol requirement. This
proves its static linkage; it is not a throughput or networking qualification.

musl has different DNS resolver and allocator behavior from glibc. GNU builds
remain available; compare tracker resolution, peer connectivity, throughput, CPU,
and memory in representative workloads before treating the two as performance
equivalents. A static executable receives C runtime fixes through a new Superseedr
build rather than through an OS libc package update.

References:

- [Rust platform support and minimum runtime versions](https://doc.rust-lang.org/rustc/platform-support.html)
- [cargo-zigbuild usage and static musl linking](https://github.com/rust-cross/cargo-zigbuild)
- [musl differences from glibc](https://wiki.musl-libc.org/functional-differences-from-glibc.html)
