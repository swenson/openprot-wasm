// Licensed under the Apache-2.0 license

//! What a scenario driver sees of the running machine.
//!
//! A driver is an `async fn` that takes a [`Host`]. The [`Machine`] runs the
//! emulator in slices of a few hundred cycles and polls the driver after each
//! one, so a driver waits by awaiting ([`Host::tick`] and the helpers built on
//! it) and never steps the emulator itself. Everything is single-threaded,
//! which is what a browser tab offers.
//!
//! [`Machine`]: crate::machine::Machine

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use caliptra_hw_model::ExitStatus;
use mcu_hw_model::{McuHwModel, ModelEmulated};
use serde::Serialize;

/// The MCU ROM sets this MCI boot milestone when it has finished its boot
/// flow, just before it jumps to the firmware.
const FIRMWARE_BOOT_FLOW_COMPLETE: u16 = 1 << 7;

/// The status the MCU firmware exited with, if it has exited: 0 for a pass.
///
/// OpenPRoT's firmware exits by writing the pass or fail byte to the UART,
/// as caliptra-mcu-sw's test firmware does. The emulator also has an exit
/// register, which takes any status.
pub(crate) fn exit_status(model: &mut ModelEmulated) -> Option<i32> {
    model
        .mcu_exit_status()
        .or_else(|| match model.output().exit_status()? {
            ExitStatus::Passed => Some(0),
            ExitStatus::Failed => Some(1),
        })
}

/// Something a driver reports to the page.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    /// The emulator's cycle count when the event was reported.
    pub cycle: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// The driver started a step. The step before it, if any, passed.
    Step { name: String },
    /// A remark on the current step.
    Note { text: String },
    /// A message the host sent to the device (`tx`) or received from it (`rx`).
    Message {
        direction: Direction,
        /// The protocol, e.g. "SPDM".
        protocol: String,
        /// The message's name, e.g. "GET_VERSION".
        name: String,
        /// The message bytes in hex.
        bytes: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Tx,
    Rx,
}

/// State shared by the machine and the driver.
pub(crate) struct Shared {
    pub(crate) model: RefCell<ModelEmulated>,
    /// Everything the UARTs have printed since reset.
    pub(crate) log: RefCell<Vec<u8>>,
    pub(crate) events: RefCell<Vec<Event>>,
    /// Counts the slices the machine has run.
    pub(crate) slice: Cell<u64>,
}

/// A driver's handle to the machine.
#[derive(Clone)]
pub struct Host {
    pub(crate) shared: Rc<Shared>,
}

/// Completes once the machine has run another slice.
struct NextSlice {
    shared: Rc<Shared>,
    from: u64,
}

impl Future for NextSlice {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.shared.slice.get() > self.from {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Host {
    /// Calls `f` with the hardware model. The model must not be stepped here:
    /// the machine does that between polls.
    pub fn with_model<R>(&self, f: impl FnOnce(&mut ModelEmulated) -> R) -> R {
        f(&mut self.shared.model.borrow_mut())
    }

    /// The emulator's cycle count.
    pub fn cycles(&self) -> u64 {
        self.with_model(|m| m.cycle_count())
    }

    /// The status the MCU firmware exited with, if it has exited.
    pub fn exit_status(&self) -> Option<i32> {
        self.with_model(exit_status)
    }

    /// Starts a step of the scenario. The page lists the steps and marks the
    /// last one as running, passed or failed.
    pub fn step(&self, name: &str) {
        self.event(EventKind::Step { name: name.into() });
    }

    /// Adds a remark to the current step.
    pub fn note(&self, text: impl Into<String>) {
        self.event(EventKind::Note { text: text.into() });
    }

    /// Reports a message exchanged with the device.
    pub fn message(&self, direction: Direction, protocol: &str, name: &str, bytes: &[u8]) {
        self.event(EventKind::Message {
            direction,
            protocol: protocol.into(),
            name: name.into(),
            bytes: bytes.iter().map(|b| format!("{b:02x}")).collect(),
        });
    }

    fn event(&self, kind: EventKind) {
        let cycle = self.cycles();
        self.shared.events.borrow_mut().push(Event { cycle, kind });
    }

    /// Waits for the machine to run another slice. Fails if the firmware has
    /// reported a fatal error.
    pub async fn tick(&self) -> Result<(), String> {
        NextSlice {
            shared: self.shared.clone(),
            from: self.shared.slice.get(),
        }
        .await;
        match self.with_model(|m| m.mci_fw_fatal_error()) {
            Some(code) => Err(format!("the firmware reported fatal error 0x{code:08x}")),
            None => Ok(()),
        }
    }

    /// Waits until `done` returns a value, for at most `timeout` cycles.
    /// `what` names the wait in the error. Also fails if the MCU firmware
    /// exits meanwhile.
    pub async fn wait_until<T>(
        &self,
        what: &str,
        timeout: u64,
        mut done: impl FnMut(&Host) -> Option<T>,
    ) -> Result<T, String> {
        let start = self.cycles();
        loop {
            if let Some(value) = done(self) {
                return Ok(value);
            }
            if let Some(status) = self.exit_status() {
                return Err(format!(
                    "the firmware exited with status {status} while the host waited for {what}"
                ));
            }
            if self.cycles() - start > timeout {
                return Err(format!(
                    "timed out after {timeout} cycles waiting for {what}"
                ));
            }
            self.tick().await?;
        }
    }

    /// Lets the machine run for `cycles` cycles.
    pub async fn run_for(&self, cycles: u64) -> Result<(), String> {
        let end = self.cycles() + cycles;
        while self.cycles() < end {
            self.tick().await?;
        }
        Ok(())
    }

    /// Waits for the MCU ROM to finish its boot flow.
    pub async fn wait_for_boot(&self, timeout: u64) -> Result<(), String> {
        self.wait_until("the MCU ROM to finish booting", timeout, |host| {
            let milestones = host.with_model(|m| u16::from(m.mci_boot_milestones()));
            (milestones & FIRMWARE_BOOT_FLOW_COMPLETE != 0).then_some(())
        })
        .await
    }

    /// Waits for `term` to appear in the UART log.
    pub async fn wait_for_log(&self, term: &str, timeout: u64) -> Result<(), String> {
        // Only the part of the log added since the last look, plus enough
        // before it to hold a match in progress, is searched.
        let window = term.len() * INTERLEAVE_SPAN;
        let mut searched = 0;
        self.wait_until(&format!("{term:?} in the log"), timeout, |host| {
            let log = host.shared.log.borrow();
            let from = searched.min(log.len()).saturating_sub(window);
            searched = log.len();
            log_contains(&log[from..], term.as_bytes()).then_some(())
        })
        .await
    }

    /// Waits for the MCU firmware to exit and returns its status.
    pub async fn wait_for_exit(&self, timeout: u64) -> Result<i32, String> {
        let start = self.cycles();
        loop {
            if let Some(status) = self.exit_status() {
                return Ok(status);
            }
            if self.cycles() - start > timeout {
                return Err(format!(
                    "timed out after {timeout} cycles waiting for the firmware to exit"
                ));
            }
            self.tick().await?;
        }
    }
}

/// A match may spread over this many times the length of the text.
const INTERLEAVE_SPAN: usize = 4;

/// Whether `needle` is in `log`. Caliptra and the MCU print to one stream, so
/// lines they print at the same time interleave byte by byte. `needle` also
/// counts as found if its bytes appear in order within a short span.
pub(crate) fn log_contains(log: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else {
        return true;
    };
    if log.windows(needle.len()).any(|w| w == needle) {
        return true;
    }
    let span = needle.len() * INTERLEAVE_SPAN;
    (0..log.len()).filter(|&i| log[i] == first).any(|start| {
        let mut matched = 0;
        for &b in &log[start..log.len().min(start + span)] {
            if b == needle[matched] {
                matched += 1;
                if matched == needle.len() {
                    return true;
                }
            }
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::log_contains;

    #[test]
    fn finds_text_in_the_log() {
        assert!(log_contains(
            b"boot\nspdm responder: ready\n",
            b"responder: ready"
        ));
        assert!(!log_contains(
            b"boot\nspdm responder\n",
            b"responder: ready"
        ));
        assert!(log_contains(b"anything", b""));
    }

    #[test]
    fn finds_text_interleaved_with_another_line() {
        assert!(log_contains(b"re[artd] yh\nello", b"ready"));
        // The bytes are all there, but too far apart to be one line.
        assert!(!log_contains(
            b"r.....................e.....................a.....d.....y",
            b"ready"
        ));
    }
}
