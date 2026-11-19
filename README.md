# openprot-wasm

Run [OpenPRoT](https://github.com/OpenPRoT/openprot)'s VeeR firmware (Caliptra
Subsystem 2.1) in a browser. The firmware runs on the Caliptra subsystem
emulator, compiled to WebAssembly as in
[caliptra-wasm](https://github.com/swenson/caliptra-wasm).

OpenPRoT does not have a reference firmware image yet, so the page runs its
test scenarios. A
scenario is one of OpenPRoT's VeeR test images plus a host driver: the part
of the test that OpenPRoT runs on the host, here compiled into the page.

| Id | Scenario | Image | Host driver |
| --- | --- | --- | --- |
| `spdm` | SPDM responder | `//target/veer/tests/spdm:spdm_image` | `spdm` |
| `flash` | Flash driver | `//target/veer/tests/flash:flash` | `on-device` |
| `flash-service` | Flash service | `//target/veer/tests/flash_service:flash_service` | `on-device` |
| `sha` | SHA driver | `//target/veer/tests/sha:sha` | `on-device` |
| `rng` | RNG driver | `//target/veer/tests/rng:rng` | `on-device` |
| `ecdsa` | ECDSA driver | `//target/veer/tests/ecdsa:ecdsa` | `on-device` |
| `dpe` | DPE driver | `//target/veer/tests/dpe:dpe` | `on-device` |
| `mci` | MCI driver | `//target/veer/tests/mci:mci` | `on-device` |
| `handoff` | ROM handoff table | `//target/veer/tests/handoff:handoff` | `on-device` |

The two drivers:

- `spdm` is an SPDM requester. It follows OpenPRoT's
  `target/veer/tests/spdm/host_test.rs`: it sends `GET_VERSION`,
  `GET_CAPABILITIES` and `NEGOTIATE_ALGORITHMS` as MCTP messages over I3C and
  checks the responses. It then checks that `GET_DIGESTS` is refused (the
  image has no certificates) and that the responder answers the request after
  that.
- `on-device` boots an image that tests itself and waits for it to exit with
  PASS or FAIL. OpenPRoT runs such images with `caliptra_on_device_test` and
  has more of them. The eight `on-device` scenarios in the table are the ones
  run here so far.

The page shows the driver's steps, the messages it exchanges, and the UART
output of Caliptra and the MCU, with the ANSI colors of OpenPRoT's log lines.
A table lists each image, the sources it was built from, and their version
(see [Sources and versions](#sources-and-versions)).
`?scenario=<id>` selects a scenario and `&run` starts it, e.g.
`http://localhost:8080/?scenario=spdm&run`.

## How it works

`build.sh` builds each scenario's image with Bazel in an OpenPRoT checkout, as
`<image>_image_emulator`: the image signed for the emulator, with the Caliptra
ROM and firmware, the MCU ROM and the SoC manifest that boot it. These are the
files OpenPRoT's own emulator tests boot.

The emulator is caliptra-mcu-sw's `ModelEmulated`, which OpenPRoT's host tests
also use. The page runs the emulator in its one thread and cannot open
sockets, so two things differ from those tests:

- The host reaches the MCU's I3C target through its queues in the emulator
  (`emulator/src/i3c.rs`), where the host tests use a TCP socket and a
  controller thread.
- A driver is an `async fn`. The page runs the emulator for a batch of cycles
  per animation frame, and the driver is polled every 500 cycles, so a driver
  waits by awaiting and the page stays responsive. `emulator/src/host.rs` has
  what a driver can await: a number of cycles, a line in the log, the end of
  the boot, the firmware's exit.

## Building

Requirements: rustup, [wasm-pack](https://rustwasm.github.io/wasm-pack/),
Bazel (or Bazelisk), Python 3 and git.

```sh
./build.sh
cd www && python3 -m http.server 8080
```

`scenarios.json` pins an OpenPRoT revision. `build.sh` clones OpenPRoT into
`~/.cache/openprot-wasm`, checks out that revision and builds the firmware
there. To build the firmware in your own checkout instead, at its current
revision:

```sh
OPENPROT_DIR=~/openprot ./build.sh
```

The build does not modify the checkout's sources. `SCENARIOS="spdm flash"`
builds some of the scenarios, and `SKIP_FIRMWARE=1` rebuilds the emulator
only.

The output goes to `www/`: `images/` (the boot files, named by content, so
scenarios share the ROMs and the Caliptra firmware), `scenarios.json` (the
scenarios, their images and the sources of the images), `pkg/` (the WASM
module) and `build-info.json` (the emulator's sources).

## Publishing

The site is at <https://swenson.github.io/openprot-wasm/>, served by GitHub
Pages from the `gh-pages` branch. To publish the site as built in `www/`:

```sh
./build.sh
node tests/browser/scenario-test.mjs
./tools/publish-pages.sh --push
```

`publish-pages.sh` commits the contents of `www/` to the local `gh-pages`
branch, on top of the published one, and with `--push` pushes it to `origin`.
It refuses to publish firmware that was built from an OpenPRoT checkout with
local changes.

## Testing

After `./build.sh`:

```sh
# Host: run every built scenario on the same emulator and drivers, natively.
(cd emulator && cargo test)

# Browser: serve www/, open the page in headless Firefox and run every
# scenario through the page. Needs Node 22+ and Firefox; no npm packages.
# --scenario spdm runs one; --out DIR saves each run's UART output, page state
# and a screenshot.
node tests/browser/scenario-test.mjs
```

`SCENARIOS="spdm"` limits the host test to some scenarios. Before sending
changes, run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
and `cargo clippy --target wasm32-unknown-unknown -- -D warnings` in
`emulator/`.

All nine scenarios pass on the host and in Firefox. A scenario runs for 14 to
26 million emulated cycles. Headless Firefox ran about 0.5 million cycles per
second on the machine this was developed on, so a scenario took 30 to 70
seconds.

## Adding a scenario

For an image that tests itself, add an entry to `scenarios.json` with the
`on-device` driver, for example:

```json
{
  "id": "wdt",
  "title": "Watchdog driver",
  "description": "What the page says about the scenario.",
  "image": "//target/veer/tests/wdt:wdt",
  "driver": "on-device"
}
```

`image` is the label of a `veer_system_image` or `veer_unittest_image`.

For a test with a host side, port the host test to a driver.
`emulator/src/scenarios/spdm.rs` is an example.

1. Add a module in `emulator/src/scenarios/` with an
   `async fn run(host: Host) -> Result<(), String>` and a `DRIVER` constant,
   and list it in `DRIVERS` in `emulator/src/scenarios/mod.rs`.
2. The page lists a driver's progress as named steps. Call `host.step(name)`
   when the driver starts one, `host.note(...)` to remark on it, and
   `host.message(...)` for each message sent or received. An `Err` fails the
   step the driver is in.
3. Use `MctpHost` (`emulator/src/mctp.rs`) to exchange MCTP messages, or
   `I3cHost` for raw I3C transfers. `host.with_model(...)` gives the hardware
   model for anything else (mailboxes, MCI registers).
4. Name the driver in the scenario's entry in `scenarios.json`.

## Layout

- `scenarios.json`: the scenarios and the OpenPRoT revision they are built
  from.
- `build.sh`, `tools/build-scenarios.py`: the build. `tools/publish-pages.sh`
  publishes its output. `bazel/` is a small Bazel
  module that the build passes to Bazel with `--inject_repository`, which adds
  no file to the OpenPRoT checkout. Its aspect puts an image's boot files in
  an output group, because `bazel build` of a `caliptra_mcu_image` alone
  builds only the `.boot_env` file.
- `emulator/`: the Rust crate. `machine.rs` is the emulator and the driver
  runner, `host.rs` the driver's view of it, `i3c.rs` and `mctp.rs` the host's
  transports, `scenarios/` the drivers, and `lib.rs` the JavaScript API.
  `cfi-shim/` stands in for caliptra-cfi-lib, whose inline assembly does not
  build for wasm32.
- `www/`: the page (`index.html`, `app.js`, `style.css`) and the build output.
- `tests/browser/`: the browser test and its WebDriver BiDi harness.

## Sources and versions

At the OpenPRoT revision that `scenarios.json` pins (`30b78eeb` on `main`), a
scenario runs:

- the Caliptra ROM, FMC and runtime built from caliptra-sw's 2.1.3 sources
  (tag `fw-2.1.3`);
- the MCU ROM built from caliptra-mcu-sw at tag `rt-sdk-2.1.1`, the Runtime
  SDK 2.1.1 release;
- the scenario's OpenPRoT test image as the MCU firmware.

OpenPRoT compiles all of them itself, with Bazel. The caliptra-sw and
caliptra-mcu-sw revisions are its pins in `third_party/caliptra/versions.bzl`.

| Image | Built from | Source version |
| --- | --- | --- |
| Caliptra ROM | chipsalliance/caliptra-sw `b99535de` (tag `fw-2.1.3`) | 2.1.3 |
| Caliptra FMC | chipsalliance/caliptra-sw `b99535de` | 2.1.3 |
| Caliptra runtime | chipsalliance/caliptra-sw `b99535de` | 2.1.3 |
| MCU ROM | chipsalliance/caliptra-mcu-sw `8c9c8ff2` (tag `rt-sdk-2.1.1`) | has none |
| MCU firmware | [OpenPRoT/openprot](https://github.com/OpenPRoT/openprot) `30b78eeb` | has none |

The OpenPRoT revision identifies the MCU firmware.

The source version of the ROM, the FMC and the runtime is each one's constant
in caliptra-sw's `builder/src/version.rs` (`ROM_VERSION_*`, `FMC_VERSION_*`
and `RUNTIME_VERSION_*`). The images that OpenPRoT builds report other numbers
in Caliptra's `CPTRA_FW_REV_ID` registers: 2.0.0 for the ROM and 0 for the FMC
and the runtime (OpenPRoT/openprot#765).

The Caliptra ROM is not caliptra-sw's released ROM binary. OpenPRoT compiles
it from the `fw-2.1.3` commit, and caliptra-sw's ROM release tag `rom-2.1.3-1`
is a different commit.

caliptra-mcu-sw releases its ROM and its runtime separately, as a ROM SDK
(tags `rom-sdk-*`) and a Runtime SDK (tags `rt-sdk-*`). OpenPRoT pins the
Runtime SDK 2.1.1 tag and builds the MCU ROM from that revision too. The MCU
ROM has no version number, and that revision is not a ROM SDK release: it is
later than ROM SDK 2.1.0 and earlier than ROM SDK 2.1.1. The revision is what
identifies the MCU ROM.

`build.sh` reads the pins and the source versions from the OpenPRoT checkout,
and the page lists them under "Images and versions".

The emulator, which runs these images, is built from other revisions:

| Part of the emulator | Repository | Revision |
| --- | --- | --- |
| MCU and subsystem model | [swenson/caliptra-mcu-sw](https://github.com/swenson/caliptra-mcu-sw/tree/wasm32-main-2.1-pr2379) `wasm32-main-2.1-pr2379` | `81c58651` |
| Caliptra core model | chipsalliance/caliptra-sw `main` | `757b7445` |

The emulator's revisions are the ones caliptra-wasm uses for Caliptra 2.1,
which build for wasm32. They do not change the images in the table above. The
emulator's caliptra-mcu-sw revision is 67 commits after OpenPRoT's pin
(`8c9c8ff2`), on the same `main-2.1` branch.
OpenPRoT's own tests run the same images on the emulator at its pin, so a
result here can differ from theirs.

## Limits

- OpenPRoT patches its emulator so that a host test can fill and drain the
  recovery FIFO (`emulator_i3c_recovery_fifo.patch`).
  `//target/veer/tests/ocp_recovery` needs that patch, and this emulator does
  not have it.
- Caliptra and the MCU print to one UART stream, so lines that they print at
  the same time interleave. `Host::wait_for_log` therefore also accepts a
  line that has characters of another line mixed into it.
- The `spdm` driver matches the host test at the pinned OpenPRoT revision.
  When the SPDM image gains certificates (OpenPRoT/openprot#762), the driver
  has to follow.
- OpenPRoT's other host-driven VeeR tests have no driver yet: `mctp_i3c`,
  `i3c_service`, `rx_timing`, `runtime` and `ocp_recovery`.
- OpenPRoT has no firmware update test for VeeR at the pinned revision.
