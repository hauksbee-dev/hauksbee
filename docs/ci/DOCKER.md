# Docker

hauksbee ships two container images. Both are multi-arch (`linux/amd64` and
`linux/arm64`) and both carry the prebuilt `hauksbee` and `hauksbee-ci`
binaries plus the reference model database, so nothing is compiled at run time.
They cover the analysis, simulation, and CI surface. The interactive 3D
frontend is deliberately not containerised (headless WebGL is pain for no
gain). Run that from a normal checkout when you want the live viewer.

## The two images

| Image | Tag | Contains | Unlocks |
|-------|-----|----------|---------|
| slim / core | `ghcr.io/hauksbee-dev/hauksbee:slim` | `hauksbee` + `hauksbee-ci`, the model db, the linked-in simavr (the AVR microcontroller emulator) | Static checks (DRC, netlint, SI, resource conflicts), board-as-code, AVR co-sim (running firmware on the emulated MCU against the live analog solve). The everyday CI image. |
| full | `ghcr.io/hauksbee-dev/hauksbee:full` | Everything in slim, plus Renode, the Espressif QEMU fork, and freerouting (with a JRE) | STM32 / nRF52 / RISC-V co-sim (Renode), ESP32 / ESP32-S3 / ESP32-C3 co-sim (Espressif QEMU), and production autorouting of recompiled boards (freerouting). |

Each is also published with the release version baked in:
`:slim-<version>` / `:full-<version>` (for example `:slim-0.1.0`), and a tag
push moves `:latest` (slim) and `:full-latest`.

### Why two

The slim image is roughly 183 MB (measured on a local amd64 build) and covers
most CI needs: the static checks and the
built-in AVR co-simulation have no external dependencies at all. Boards are
read through the vendored forge crates, not through KiCad.
The full image adds the heavy external backends (Renode and the Espressif
QEMU fork are large prebuilt toolchains, freerouting needs a JRE). Pull it
only when you actually run STM32 / ESP32 firmware or autoroute a board.

The full image does **not** include esp-idf or any firmware toolchain. You
need esp-idf only to *build* ESP32 firmware. At run time the Espressif QEMU
binary plus a merged flash image are enough, and esp-idf is gigabytes. Compile
your firmware elsewhere (or in a separate build step) and feed the artifact
in.

### Backend wiring (full image)

hauksbee finds the co-sim backends at run time through the `HAUKSBEE_*` env
vars. The full image sets these so everything works with no setup:

| Variable | Value | Backend |
|----------|-------|---------|
| `HAUKSBEE_RENODE` | `/usr/local/bin/renode` | Renode (STM32 / nRF52 / RISC-V) |
| `HAUKSBEE_QEMU_DIR` | `/opt/qemu/bin` | Espressif QEMU (both arches) |
| `HAUKSBEE_QEMU_XTENSA` | `/opt/qemu/bin/qemu-system-xtensa` | ESP32 / ESP32-S3 |
| `HAUKSBEE_QEMU_RISCV32` | `/opt/qemu/bin/qemu-system-riscv32` | ESP32-C3 |
| `FREEROUTING_JAR` | `/opt/freerouting.jar` | freerouting autorouter |

## `docker run` examples

The images mount your working tree at `/work` and have no fixed entrypoint, so
the command names the tool (`hauksbee` for the engine, `hauksbee-ci` for the
CI runner). The bundled example boards live inside the image, under the
binaries' embedded data. The boards and specs you check usually come from
your own repo, mounted in.

Report a board (the bind report table):

```bash
docker run --rm -v "$PWD:/work" ghcr.io/hauksbee-dev/hauksbee:slim \
  hauksbee run path/to/board.kicad_pcb --report
```

Geometric short / clearance DRC:

```bash
docker run --rm -v "$PWD:/work" ghcr.io/hauksbee-dev/hauksbee:slim \
  hauksbee run path/to/board.kicad_pcb --drc
```

Run a hauksbee-ci spec and write JUnit (the CI flow). On a CI runner the
working tree is usually owned by the runner user, not the image's `hauksbee`
user (uid 1000), so add `--user "$(id -u):$(id -g)"` whenever the run writes
output (a JUnit file, a routed board) back into the mount:

```bash
docker run --rm -v "$PWD:/work" --user "$(id -u):$(id -g)" \
  ghcr.io/hauksbee-dev/hauksbee:slim \
  hauksbee-ci run ci/watchy.toml --junit hauksbee-ci-results.xml
```

AVR firmware co-sim (built into the slim image):

```bash
docker run --rm -v "$PWD:/work" ghcr.io/hauksbee-dev/hauksbee:slim \
  hauksbee run board.kicad_pcb --firmware fw/firmware.hex --headless --seconds 5
```

ESP32 / STM32 co-sim and autorouting need the full image:

```bash
# ESP32 boot_coverage spec (Espressif QEMU, in the full image)
docker run --rm -v "$PWD:/work" ghcr.io/hauksbee-dev/hauksbee:full \
  hauksbee-ci run ci/esp32_boot.toml --junit results.xml

# Recompile board-as-code and autoroute it (freerouting, in the full image)
docker run --rm -v "$PWD:/work" ghcr.io/hauksbee-dev/hauksbee:full \
  hauksbee from-code board.board --out routed.kicad_pcb --route
```

## Using it from the GitHub Action

`integrations/github-action/action.yml` can run hauksbee-ci straight from the
published image instead of downloading a release binary or compiling from
source. Set `use-image: true`, and optionally pick the image:

```yaml
- uses: actions/checkout@v4
  with:
    repository: hauksbee-dev/hauksbee
    ref: v0.1.0
    path: .hauksbee-action
    token: ${{ secrets.HAUKSBEE_READ_TOKEN }}
- uses: ./.hauksbee-action/integrations/github-action
  with:
    hauksbee-token: ${{ secrets.HAUKSBEE_READ_TOKEN }}
    spec: ci/watchy.toml
    use-image: true
    # default is ghcr.io/hauksbee-dev/hauksbee:slim; use :full for ESP32 / STM32
    # co-sim or autorouting.
    image: ghcr.io/hauksbee-dev/hauksbee:full
```

When `use-image` is true the Action mounts the checkout at `/work`, runs the
container as the runner user so the JUnit XML is writable, and publishes the
results to the Checks tab exactly as the binary path does. When it is false,
the Action keeps its existing behaviour: prefer a prebuilt release binary, fall
back to `cargo build`.

## How the images are built

`.github/workflows/docker.yml` builds and pushes the images on a release tag
(the same `v*` tag that drives `release.yml`). It uses the standard
`docker/setup-qemu-action` + `docker/setup-buildx-action` +
`docker/login-action` + `docker/build-push-action` chain and pushes multi-arch
manifests to GHCR.

The build is **multi-stage from source**, not a repackaged release tarball:

1. A stage builds simavr (the GPL AVR core) into a static lib, because the
   `avr` feature links it.
2. A `rust:bookworm` stage builds `hauksbee` and `hauksbee-ci` with
   `cargo build --release` and strips them.
3. A `debian:bookworm-slim` runtime stage `COPY`s in just the two binaries and
   the model db (simavr is statically linked into the binaries); the shared
   libraries it still needs, `libelf1` and `zlib1g`, come from apt.

This choice of multi-stage from source over consuming `scripts/bundle.sh`'s
tarball has two reasons. First, each architecture builds natively under
buildx (`linux/amd64` and `linux/arm64`), with no cross-compilation and no
dependency on a release tarball existing first. Second, simavr compiles per
arch the same way, so the linked-in AVR backend is the one that actually ran
on that arch. The full image then builds `FROM` the pushed slim image and
layers the prebuilt Renode / Espressif QEMU / freerouting downloads on top,
selecting the right per-arch asset from `TARGETARCH`.

### Build context

The `forge-*` crates are vendored into this repo (`vendor/kicad-forge`, see
`vendor/kicad-forge/VENDORED.md`), so the build is self-contained. The Docker
build context is a parent directory that holds just this repo:

```
ctx/
  hauksbee/      this repo (incl. vendor/kicad-forge)
```

The CI workflow checks it out into `ctx/` and writes a `.dockerignore` there
to keep the frontend and other build-irrelevant trees out of the context.

## Build it yourself and smoke-test

CI really produces the images (that is where the multi-arch manifests get
built and pushed). To build locally, stage this repo under a context dir,
then build each image. These are the exact commands:

```bash
# 1. Stage this repo under a context dir (forge crates are vendored inside it).
#    The directory MUST be named `hauksbee`: the Dockerfile copies that path.
mkdir -p ctx
git clone https://github.com/hauksbee-dev/hauksbee.git ctx/hauksbee

#    From a checkout you already have, export it rather than symlinking:
#    docker does not follow symlinks out of the build context, so a link here
#    fails with "/hauksbee: not found" partway through the build.
#    mkdir -p ctx/hauksbee && git -C /path/to/checkout archive HEAD | tar -x -C ctx/hauksbee

# 2. Build the slim image for your host arch.
docker build \
  -f ctx/hauksbee/docker/Dockerfile.slim \
  -t hauksbee:slim \
  ctx

# 3. Build the full image FROM the local slim image.
docker build \
  -f ctx/hauksbee/docker/Dockerfile.full \
  --build-arg SLIM_IMAGE=hauksbee:slim \
  -t hauksbee:full \
  ctx
```

Smoke-test the result:

```bash
# Binaries present and runnable.
docker run --rm hauksbee:slim hauksbee --help
docker run --rm hauksbee:slim hauksbee-ci --help

# Full image: the co-sim backends resolve.
docker run --rm hauksbee:full renode --version
docker run --rm hauksbee:full /opt/qemu/bin/qemu-system-xtensa --version
docker run --rm hauksbee:full java -jar /opt/freerouting.jar --help

# Run a real check against a board in your checkout.
docker run --rm -v "$PWD:/work" hauksbee:slim \
  hauksbee run crates/hauksbee-ci/examples/boards/watchy.kicad_pcb --report
```

For a multi-arch local build (both arches at once, without pushing) use
buildx. A two-platform build cannot be exported with `--load` (the classic
docker image store holds one platform per tag), so either enable the
containerd image store in Docker's settings, or create a `docker-container`
builder and keep the result in the build cache / push it to a registry:

```bash
docker buildx create --name hauksbee-builder --driver docker-container --use
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -f ctx/hauksbee/docker/Dockerfile.slim \
  -t hauksbee:slim \
  ctx
```

Building `linux/arm64` on an amd64 host (or vice versa) goes through QEMU
emulation. This is slow, but it is exactly what the CI workflow does.
