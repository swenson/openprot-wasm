// Licensed under the Apache-2.0 license

//! The host-side drivers of the test scenarios.
//!
//! A scenario is a firmware image plus the driver that plays the host's part
//! and decides whether the run passed. `scenarios.json` at the top of the
//! repository pairs each image with a driver by name.
//!
//! To add a driver, write an `async fn(Host) -> Result<(), String>` in a new
//! module here and list it in [`DRIVERS`].

use std::future::Future;
use std::pin::Pin;

use crate::host::Host;

mod on_device;
mod pldm;
mod spdm;

/// A running driver. It resolves when the scenario has passed or failed.
pub type ScenarioFuture = Pin<Box<dyn Future<Output = Result<(), String>>>>;

/// A host-side driver.
pub struct Driver {
    /// The name `scenarios.json` refers to the driver by.
    pub name: &'static str,
    /// What the driver does, in a sentence.
    pub description: &'static str,
    pub start: fn(Host) -> ScenarioFuture,
}

pub const DRIVERS: &[Driver] = &[on_device::DRIVER, pldm::DRIVER, spdm::DRIVER];

pub fn find(name: &str) -> Option<&'static Driver> {
    DRIVERS.iter().find(|driver| driver.name == name)
}

/// Cycles to wait for Caliptra and the MCU ROM to boot to the firmware.
const BOOT_TIMEOUT_CYCLES: u64 = 200_000_000;
