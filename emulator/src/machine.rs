// Licensed under the Apache-2.0 license

//! The emulated Caliptra subsystem and the scenario driver that runs against
//! it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use caliptra_api::SocManager;
use caliptra_hw_model_types::{EtrngResponse, RandomEtrngResponses, RandomNibbles};
use mcu_hw_model::{Fuses, InitParams, McuHwModel, ModelEmulated};
use rand::rngs::StdRng;
use rand::SeedableRng;

use crate::host::{exit_status, Event, Host, Shared};
use crate::scenarios::{self, ScenarioFuture};

/// The driver is polled after every this many cycles.
const SLICE_CYCLES: u32 = 500;

/// Seed of the emulated TRNG. Fixed, so that a run is repeatable.
const TRNG_SEED: u64 = 42;

/// The images a scenario boots: what OpenPRoT's `caliptra_mcu_image` rule
/// lists in its `.boot_env` file.
pub struct BootImages<'a> {
    pub caliptra_rom: &'a [u8],
    pub caliptra_fw: &'a [u8],
    pub soc_manifest: &'a [u8],
    pub mcu_rom: &'a [u8],
    pub mcu_fw: &'a [u8],
    /// SHA-384 of the vendor public keys that signed the Caliptra firmware.
    pub vendor_pk_hash: [u8; 48],
}

/// The versions that Caliptra's ROM and firmware report in the
/// `CPTRA_FW_REV_ID` registers, as "major.minor.patch". These are the numbers
/// the image build stamped on the images, which can differ from the version
/// of the sources.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaliptraVersions {
    /// `None` until the ROM has written its version.
    pub rom: Option<String>,
    /// `None` until the runtime is ready. A firmware bundle stamped with
    /// version 0 reads "0.0.0".
    pub fmc: Option<String>,
    pub runtime: Option<String>,
}

impl CaliptraVersions {
    /// Decodes `CPTRA_FW_REV_ID[0]` (the ROM version in the low half and the
    /// FMC version in the high half) and `CPTRA_FW_REV_ID[1]` (the runtime
    /// version). The firmware's registers are final once the runtime is
    /// ready.
    fn from_registers(rev_id_0: u32, rev_id_1: u32, runtime_ready: bool) -> Self {
        // ROM and FMC: 5 bits of major, 5 of minor, 6 of patch.
        let short = |v: u32| format!("{}.{}.{}", (v >> 11) & 0x1f, (v >> 6) & 0x1f, v & 0x3f);
        // Runtime: 8 bits of major, 8 of minor, 16 of patch.
        let long = |v: u32| format!("{}.{}.{}", v >> 24, (v >> 16) & 0xff, v & 0xffff);
        let rom = rev_id_0 & 0xffff;
        Self {
            rom: (rom != 0).then(|| short(rom)),
            fmc: runtime_ready.then(|| short(rev_id_0 >> 16)),
            runtime: runtime_ready.then(|| long(rev_id_1)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// The driver has not finished.
    Running,
    Passed,
    Failed(String),
}

pub struct Machine {
    shared: Rc<Shared>,
    /// The driver, until it finishes.
    scenario: Option<ScenarioFuture>,
    state: State,
    /// UART output that `take_uart` has not returned yet.
    uart: String,
    /// How many of the driver's events `take_events` has returned.
    events_taken: usize,
}

impl Machine {
    /// Creates the machine, releases it from reset and starts `driver`.
    ///
    /// The model is set up as OpenPRoT's host tests set it up
    /// (`target/veer/tooling/test_boot_files.rs`), except that booting does
    /// not block: the driver waits for the boot instead.
    pub fn new(images: &BootImages, driver: &str) -> Result<Self, String> {
        let driver =
            scenarios::find(driver).ok_or_else(|| format!("no driver named {driver:?}"))?;

        // Fuses take the vendor PK hash as big-endian words.
        let mut fuse_hash = [0u32; 12];
        for (word, chunk) in fuse_hash
            .iter_mut()
            .zip(images.vendor_pk_hash.chunks_exact(4))
        {
            *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        let itrng_nibbles: Box<dyn Iterator<Item = u8> + Send> =
            Box::new(RandomNibbles(StdRng::seed_from_u64(TRNG_SEED)));
        let etrng_responses: Box<dyn Iterator<Item = EtrngResponse> + Send> =
            Box::new(RandomEtrngResponses(StdRng::seed_from_u64(TRNG_SEED)));

        let params = InitParams {
            fuses: Fuses {
                vendor_pk_hash: fuse_hash,
                ..Default::default()
            },
            caliptra_rom: images.caliptra_rom,
            caliptra_firmware: images.caliptra_fw,
            soc_manifest: images.soc_manifest,
            mcu_rom: images.mcu_rom,
            mcu_firmware: images.mcu_fw,
            vendor_pk_hash: Some(images.vendor_pk_hash),
            active_mode: true,
            enable_mcu_uart_log: true,
            check_booted_to_runtime: false,
            // The UART text is read from the model's output instead.
            log_writer: Box::new(std::io::sink()),
            itrng_nibbles,
            etrng_responses,
            random_sram_puf: false,
            // A write to the emulator's exit register is recorded, and halts
            // the MCU, instead of exiting the process.
            capture_mcu_exit: true,
            // Caliptra's UART would otherwise go to stdout.
            caliptra_uart_to_output: true,
            ..Default::default()
        };
        let mut model = ModelEmulated::new_unbooted(params)
            .map_err(|e| format!("creating the emulator: {e}"))?;
        model
            .boot()
            .map_err(|e| format!("starting the emulator: {e}"))?;

        let shared = Rc::new(Shared {
            model: RefCell::new(model),
            log: RefCell::new(Vec::new()),
            events: RefCell::new(Vec::new()),
            slice: Cell::new(0),
        });
        let scenario = (driver.start)(Host {
            shared: shared.clone(),
        });
        let mut machine = Self {
            shared,
            scenario: Some(scenario),
            state: State::Running,
            uart: String::new(),
            events_taken: 0,
        };
        // Lets the driver report its first step before the first cycle.
        machine.poll_scenario();
        Ok(machine)
    }

    /// Runs the emulator for `cycles` cycles, polling the driver as it goes.
    /// The emulator keeps running after the driver has finished.
    pub fn run(&mut self, cycles: u32) {
        let mut left = cycles;
        while left > 0 {
            let slice = left.min(SLICE_CYCLES);
            left -= slice;
            {
                let mut model = self.shared.model.borrow_mut();
                for _ in 0..slice {
                    model.step();
                }
                let out = model.output().take(usize::MAX);
                if !out.is_empty() {
                    self.shared
                        .log
                        .borrow_mut()
                        .extend_from_slice(out.as_bytes());
                    self.uart.push_str(&out);
                }
            }
            self.shared.slice.set(self.shared.slice.get() + 1);
            self.poll_scenario();
        }
    }

    fn poll_scenario(&mut self) {
        let Some(scenario) = self.scenario.as_mut() else {
            return;
        };
        let mut cx = Context::from_waker(Waker::noop());
        if let Poll::Ready(result) = scenario.as_mut().poll(&mut cx) {
            self.scenario = None;
            self.state = match result {
                Ok(()) => State::Passed,
                Err(e) => State::Failed(e),
            };
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// The emulator's cycle count.
    pub fn cycles(&self) -> u64 {
        self.shared.model.borrow_mut().cycle_count()
    }

    /// The status the MCU firmware exited with, if it has exited: 0 for a
    /// pass.
    pub fn mcu_exit_status(&self) -> Option<i32> {
        exit_status(&mut self.shared.model.borrow_mut())
    }

    /// The fatal error the MCU firmware has reported in MCI, if any.
    pub fn fatal_error(&self) -> Option<u32> {
        self.shared.model.borrow_mut().mci_fw_fatal_error()
    }

    /// The versions that Caliptra's ROM, FMC and runtime have reported.
    pub fn caliptra_versions(&self) -> CaliptraVersions {
        let mut model = self.shared.model.borrow_mut();
        let mut soc = model.caliptra_soc_manager();
        let rev_id_0 = soc.soc_ifc().cptra_fw_rev_id().at(0).read();
        let rev_id_1 = soc.soc_ifc().cptra_fw_rev_id().at(1).read();
        let runtime_ready = soc.soc_ifc().cptra_flow_status().read().ready_for_runtime();
        CaliptraVersions::from_registers(rev_id_0, rev_id_1, runtime_ready)
    }

    /// The UART output of Caliptra and the MCU since the last call.
    pub fn take_uart(&mut self) -> String {
        std::mem::take(&mut self.uart)
    }

    /// The events the driver has reported since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        let events = self.shared.events.borrow();
        let new = events[self.events_taken..].to_vec();
        self.events_taken = events.len();
        new
    }
}

#[cfg(test)]
mod tests {
    use super::CaliptraVersions;

    #[test]
    fn decodes_caliptra_versions() {
        // ROM 2.1.3 and FMC 2.1.5 in one register, runtime 2.1.5 in the other.
        let rom = (2 << 11) | (1 << 6) | 3;
        let fmc = (2 << 11) | (1 << 6) | 5;
        let versions = CaliptraVersions::from_registers((fmc << 16) | rom, 0x0201_0005, true);
        assert_eq!(versions.rom.as_deref(), Some("2.1.3"));
        assert_eq!(versions.fmc.as_deref(), Some("2.1.5"));
        assert_eq!(versions.runtime.as_deref(), Some("2.1.5"));
    }

    #[test]
    fn firmware_versions_wait_for_the_runtime() {
        let rom = 2 << 11;
        let versions = CaliptraVersions::from_registers(rom, 0, false);
        assert_eq!(versions.rom.as_deref(), Some("2.0.0"));
        assert_eq!(versions.fmc, None);
        assert_eq!(versions.runtime, None);

        // A bundle stamped with version 0.
        let versions = CaliptraVersions::from_registers(rom, 0, true);
        assert_eq!(versions.fmc.as_deref(), Some("0.0.0"));
        assert_eq!(versions.runtime.as_deref(), Some("0.0.0"));

        assert_eq!(CaliptraVersions::from_registers(0, 0, false).rom, None);
    }
}
