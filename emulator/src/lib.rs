// Licensed under the Apache-2.0 license

//! Runs OpenPRoT's VeeR firmware on the Caliptra subsystem emulator, with the
//! host's side of each test scenario built in, for a web page to drive.
//!
//! [`Emulator`] is the JavaScript API. [`machine::Machine`] is the same thing
//! for Rust callers, which the host tests use.

use serde::Serialize;
use wasm_bindgen::prelude::*;

pub mod host;
pub mod i3c;
pub mod machine;
pub mod mctp;
pub mod scenarios;

use machine::{BootImages, Machine, State};

/// Parses the vendor PK hash of a `.boot_env` file: 96 hex digits.
pub fn parse_vendor_pk_hash(hex: &str) -> Result<[u8; 48], String> {
    let hex = hex.trim();
    if hex.len() != 96 || !hex.is_ascii() {
        return Err(format!(
            "the vendor PK hash must be 96 hex digits, got {} characters",
            hex.len()
        ));
    }
    let mut hash = [0u8; 48];
    for (byte, digits) in hash.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let digits = std::str::from_utf8(digits).map_err(|e| e.to_string())?;
        *byte = u8::from_str_radix(digits, 16)
            .map_err(|e| format!("the vendor PK hash is not hex: {e}"))?;
    }
    Ok(hash)
}

#[derive(Serialize)]
struct DriverInfo {
    name: &'static str,
    description: &'static str,
}

/// The scenario drivers built into this module, as a JSON array of
/// `{name, description}`.
#[wasm_bindgen]
pub fn drivers() -> String {
    let list: Vec<DriverInfo> = scenarios::DRIVERS
        .iter()
        .map(|d| DriverInfo {
            name: d.name,
            description: d.description,
        })
        .collect();
    serde_json::to_string(&list).unwrap_or_default()
}

#[derive(Serialize)]
struct Status {
    /// "running", "passed" or "failed".
    state: &'static str,
    /// Why the scenario failed.
    error: Option<String>,
    cycles: u64,
    mcu_exit_status: Option<i32>,
    fatal_error: Option<u32>,
    /// The versions Caliptra reports: the ROM's once the ROM has started,
    /// the others once the runtime is ready. `null` before that.
    caliptra_rom_version: Option<String>,
    caliptra_fmc_version: Option<String>,
    caliptra_runtime_version: Option<String>,
}

/// The emulated subsystem running one scenario.
#[wasm_bindgen]
pub struct Emulator {
    machine: Machine,
}

#[wasm_bindgen]
impl Emulator {
    /// Boots the images of a scenario and starts its driver.
    ///
    /// # Arguments
    /// * `caliptra_rom` - The Caliptra ROM
    /// * `caliptra_fw` - The Caliptra firmware bundle
    /// * `soc_manifest` - The SoC manifest that authorizes `mcu_fw`
    /// * `mcu_rom` - The MCU ROM
    /// * `mcu_fw` - The MCU firmware: the OpenPRoT image
    /// * `vendor_pk_hash` - The vendor PK hash, 96 hex digits
    /// * `driver` - The name of the scenario driver (see `drivers`)
    #[wasm_bindgen(constructor)]
    pub fn new(
        caliptra_rom: &[u8],
        caliptra_fw: &[u8],
        soc_manifest: &[u8],
        mcu_rom: &[u8],
        mcu_fw: &[u8],
        vendor_pk_hash: &str,
        driver: &str,
    ) -> Result<Emulator, JsValue> {
        console_error_panic_hook::set_once();
        let images = BootImages {
            caliptra_rom,
            caliptra_fw,
            soc_manifest,
            mcu_rom,
            mcu_fw,
            vendor_pk_hash: parse_vendor_pk_hash(vendor_pk_hash)
                .map_err(|e| JsValue::from_str(&e))?,
        };
        let machine = Machine::new(&images, driver).map_err(|e| JsValue::from_str(&e))?;
        Ok(Emulator { machine })
    }

    /// Runs the emulator for `cycles` cycles.
    pub fn run(&mut self, cycles: u32) {
        self.machine.run(cycles);
    }

    /// The UART output of Caliptra and the MCU since the last call.
    pub fn take_uart(&mut self) -> String {
        self.machine.take_uart()
    }

    /// The events the driver has reported since the last call, as a JSON
    /// array. Each has a `cycle` and a `kind`: `step` (with `name`), `note`
    /// (with `text`) or `message` (with `direction`, `protocol`, `name` and
    /// `bytes` in hex).
    pub fn take_events(&mut self) -> String {
        serde_json::to_string(&self.machine.take_events()).unwrap_or_default()
    }

    /// The state of the run as JSON: `state` ("running", "passed" or
    /// "failed"), `error`, `cycles`, `mcu_exit_status`, `fatal_error`, and
    /// the versions Caliptra has reported (`caliptra_rom_version`,
    /// `caliptra_fmc_version`, `caliptra_runtime_version`).
    pub fn status(&self) -> String {
        let (state, error) = match self.machine.state() {
            State::Running => ("running", None),
            State::Passed => ("passed", None),
            State::Failed(e) => ("failed", Some(e.clone())),
        };
        let versions = self.machine.caliptra_versions();
        let status = Status {
            state,
            error,
            cycles: self.machine.cycles(),
            mcu_exit_status: self.machine.mcu_exit_status(),
            fatal_error: self.machine.fatal_error(),
            caliptra_rom_version: versions.rom,
            caliptra_fmc_version: versions.fmc,
            caliptra_runtime_version: versions.runtime,
        };
        serde_json::to_string(&status).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_vendor_pk_hash() {
        let hex = "b17ca877666657ccd100e6926c7206b60c995cb68992c6c9\
                   baefce728af05441dee1ff415adfc187e1e4edb4d3b2d909";
        let hash = parse_vendor_pk_hash(hex).unwrap();
        assert_eq!(hash[..3], [0xb1, 0x7c, 0xa8]);
        assert_eq!(hash[47], 0x09);
        assert!(parse_vendor_pk_hash("b17c").is_err());
        assert!(parse_vendor_pk_hash(&"zz".repeat(48)).is_err());
    }

    #[test]
    fn lists_the_drivers() {
        let list: serde_json::Value = serde_json::from_str(&drivers()).unwrap();
        let names: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"spdm"));
        assert!(names.contains(&"on-device"));
    }
}
