// Licensed under the Apache-2.0 license

//! The emulated Caliptra subsystem and the scenario driver that runs against
//! it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use caliptra_hw_model_types::{EtrngResponse, RandomEtrngResponses, RandomNibbles};
use mcu_hw_model::{Fuses, InitParams, McuHwModel, ModelEmulated};
use rand::rngs::StdRng;
use rand::SeedableRng;

use crate::host::{exit_status, Event, Host, Shared, UpdateImages};
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
    /// The SoC manifest and the MCU firmware of the scenario's firmware
    /// update, if it has one: the boot files of a second image, which the
    /// driver sends to the first.
    pub update: Option<(&'a [u8], &'a [u8])>,
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
            update: images.update.map(|(soc_manifest, mcu_fw)| UpdateImages {
                soc_manifest: soc_manifest.into(),
                mcu_fw: mcu_fw.into(),
            }),
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
