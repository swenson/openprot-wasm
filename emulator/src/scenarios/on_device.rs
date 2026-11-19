// Licensed under the Apache-2.0 license

//! Driver for firmware that tests itself: the images OpenPRoT runs with
//! `caliptra_on_device_test`. The host boots the image and waits for it to
//! exit. Status 0 is a pass.

use super::{Driver, ScenarioFuture, BOOT_TIMEOUT_CYCLES};
use crate::host::Host;

pub const DRIVER: Driver = Driver {
    name: "on-device",
    description: "Boots the image and waits for the firmware to exit with PASS or FAIL.",
    start,
};

/// Cycles to wait for the firmware to exit once it has started.
const TEST_TIMEOUT_CYCLES: u64 = 400_000_000;

fn start(host: Host) -> ScenarioFuture {
    Box::pin(run(host))
}

async fn run(host: Host) -> Result<(), String> {
    host.step("Boot Caliptra and the MCU ROM");
    host.wait_for_boot(BOOT_TIMEOUT_CYCLES).await?;

    host.step("Run the firmware's tests");
    match host.wait_for_exit(TEST_TIMEOUT_CYCLES).await? {
        0 => {
            host.note("The firmware exited with PASS.");
            Ok(())
        }
        status => Err(format!("the firmware exited with status {status}")),
    }
}
