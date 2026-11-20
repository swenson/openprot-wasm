// Licensed under the Apache-2.0 license

//! Runs the scenarios that `build.sh` put in `www/` on the host: the same
//! images, emulator and drivers as in the browser.
//!
//! `SCENARIOS` (ids separated by spaces) limits the run to some of them.

use std::path::{Path, PathBuf};

use openprot_wasm::host::EventKind;
use openprot_wasm::machine::{BootImages, Machine, State};
use openprot_wasm::parse_vendor_pk_hash;

/// The most cycles a scenario may take.
const MAX_CYCLES: u64 = 1_000_000_000;

/// Cycles per call to `Machine::run`, about what the page runs per frame.
const BATCH_CYCLES: u32 = 200_000;

/// Cycles to keep the emulator running after the scenario has passed.
const SETTLE_CYCLES: u64 = 2_000_000;

fn www() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../www")
}

fn run_scenario(scenario: &serde_json::Value) {
    let id = scenario["id"].as_str().unwrap();
    let image = |name: &str| {
        let path = www().join(scenario["images"][name]["path"].as_str().unwrap());
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    };
    let (caliptra_rom, caliptra_fw) = (image("caliptra_rom"), image("caliptra_fw"));
    let (mcu_rom, mcu_fw) = (image("mcu_rom"), image("mcu_fw"));
    let soc_manifest = image("soc_manifest");
    let images = BootImages {
        caliptra_rom: &caliptra_rom,
        caliptra_fw: &caliptra_fw,
        soc_manifest: &soc_manifest,
        mcu_rom: &mcu_rom,
        mcu_fw: &mcu_fw,
        vendor_pk_hash: parse_vendor_pk_hash(scenario["vendor_pk_hash"].as_str().unwrap()).unwrap(),
    };
    // On the host the model keeps flash contents in these files in the
    // working directory. A browser starts every run with erased flash.
    for flash in ["primary_flash", "secondary_flash"] {
        let _ = std::fs::remove_file(flash);
    }
    let mut machine = Machine::new(&images, scenario["driver"].as_str().unwrap()).unwrap();

    let mut uart = String::new();
    let mut passed_at = None;
    loop {
        machine.run(BATCH_CYCLES);
        uart.push_str(&machine.take_uart());
        for event in machine.take_events() {
            match event.kind {
                EventKind::Step { name } => println!("[{id}] {:>11} step: {name}", event.cycle),
                EventKind::Note { text } => println!("[{id}] {:>11}   {text}", event.cycle),
                EventKind::Message {
                    direction,
                    protocol,
                    name,
                    bytes,
                } => println!(
                    "[{id}] {:>11}   {direction:?} {protocol} {name}: {bytes}",
                    event.cycle
                ),
                EventKind::Certificate { name, certificate } => println!(
                    "[{id}] {:>11}   {name} certificate: subject {:?}, issuer {:?}. {}",
                    event.cycle,
                    certificate.subject_common_name().unwrap_or("?"),
                    certificate.issuer_common_name().unwrap_or("?"),
                    certificate.signature_check.text
                ),
            }
        }
        let tail = || &uart[uart.len().saturating_sub(4000)..];
        match machine.state() {
            State::Failed(e) => panic!(
                "scenario {id} failed after {} cycles: {e}\nUART tail:\n{}",
                machine.cycles(),
                tail()
            ),
            State::Passed if passed_at.is_none() => {
                println!("[{id}] passed after {} cycles", machine.cycles());
                passed_at = Some(machine.cycles());
            }
            _ => {}
        }
        // The emulator must keep running after the driver has finished.
        if passed_at.is_some_and(|at| machine.cycles() >= at + SETTLE_CYCLES) {
            assert_eq!(
                machine.fatal_error(),
                None,
                "fatal error after the scenario passed"
            );
            return;
        }
        assert!(
            machine.cycles() < MAX_CYCLES,
            "scenario {id} still running after {MAX_CYCLES} cycles\nUART tail:\n{}",
            tail()
        );
    }
}

#[test]
fn built_scenarios_pass() {
    let path = www().join("scenarios.json");
    let manifest = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {} (run ./build.sh first): {e}", path.display()));
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let only = std::env::var("SCENARIOS").unwrap_or_default();
    let only: Vec<&str> = only.split_whitespace().collect();

    // The flash files go in a directory of their own.
    let work = std::env::temp_dir().join(format!("openprot-wasm-test-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    std::env::set_current_dir(&work).unwrap();

    let mut ran = 0;
    for scenario in manifest["scenarios"].as_array().unwrap() {
        if only.is_empty() || only.contains(&scenario["id"].as_str().unwrap()) {
            run_scenario(scenario);
            ran += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&work);
    assert!(ran > 0, "no scenario to run");
}
