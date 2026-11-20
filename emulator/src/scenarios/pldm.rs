// Licensed under the Apache-2.0 license

//! Driver for OpenPRoT's VeeR PLDM firmware device image
//! (`//target/veer/tests/pldm:pldm_image`): the PLDM update agent.
//!
//! It follows `target/veer/tests/pldm/host_test.rs` in OpenPRoT. Each PLDM
//! message goes to or comes from the device as an MCTP message over I3C. The
//! update has two components, the boot files of a second image
//! (`//target/veer/tests/pldm/updated`): its SoC manifest and its MCU
//! firmware. The driver:
//!
//! 1. reads the device's identifiers and firmware parameters
//!    (`QueryDeviceIdentifiers`, `GetFirmwareParameters`);
//! 2. starts the update and describes the components (`RequestUpdate`,
//!    `PassComponentTable`);
//! 3. for the manifest and then the firmware, sends `UpdateComponent` and
//!    answers the requests that the device sends: `RequestFirmwareData`
//!    until the component is transferred, then `TransferComplete`,
//!    `VerifyComplete` and `ApplyComplete`;
//! 4. sends `ActivateFirmware`;
//! 5. waits for the updated firmware to run and exit with PASS.
//!
//! The device stages the components in memory that Caliptra can read. It
//! asks Caliptra to check the signatures of the manifest, and checks the
//! firmware against the digest in the manifest. To activate the update it
//! gives Caliptra the manifest and asks it to activate the firmware.
//! Caliptra resets the MCU, copies the firmware to MCU SRAM, authorizes it
//! and releases the MCU.
//!
//! The messages are written out byte by byte and the response fields are
//! read at fixed offsets (DSP0267 1.3), as in the host test.

use super::{Driver, ScenarioFuture, BOOT_TIMEOUT_CYCLES};
use crate::host::{Direction, Host};
use crate::mctp::{DeviceRequest, MctpHost};

pub const DRIVER: Driver = Driver {
    name: "pldm",
    description: "PLDM update agent over MCTP over I3C: sends a SoC manifest and an MCU firmware image to the firmware device, activates them and waits for the new firmware to run.",
    start,
};

/// The device's EID and the host's (`OWN_EID` and `HOST_EID` in OpenPRoT's
/// `target/veer/tests/mctp_i3c/ids.rs`).
const DEVICE_EID: u8 = 8;
const HOST_EID: u8 = 9;

/// The MCTP message type of PLDM (DSP0239).
const PLDM_MSG_TYPE: u8 = 1;

/// The most message bytes the driver puts in one packet. The host test
/// writes packets of the same size.
const PACKET_BODY: usize = 240;

/// Cycles the driver leaves the device after each packet it writes. The
/// update is 120 parts of three packets each, so the default gap of 150,000
/// cycles would make up most of the run. The emulated I3C target took the
/// packets of a whole update with gaps of 5,000 cycles.
const WRITE_GAP_CYCLES: u64 = 30_000;

// What the firmware device reports about itself and logs
// (`target/veer/tests/pldm/ids.rs` in OpenPRoT).
const DEVICE_UUID: [u8; 16] = *b"OPRTveerpldmdemo";
const RUNTIME_CLASSIFICATION: u16 = 0x000a;
const RUNTIME_IDENTIFIER: u16 = 0x0001;
const MANIFEST_CLASSIFICATION: u16 = 0x0001;
const MANIFEST_IDENTIFIER: u16 = 0x0002;
const ACTIVE_VERSION: &str = "1.0.0";
const ACTIVE_COMPARISON_STAMP: u32 = 1;
const READY: &str = "pldm fd: waiting for request";
const UPDATED: &str = "pldm update: updated image running";

/// Logged by the firmware's control responder once its listener is armed.
const CONTROL_READY: &str = "mctp control: responder ready";

/// The PLDM type of firmware update (DSP0245).
const PLDM_TYPE_FW_UPDATE: u8 = 5;

/// The request bit of the first byte of a PLDM header.
const PLDM_REQUEST: u8 = 0x80;

// Firmware update commands (DSP0267).
const QUERY_DEVICE_IDENTIFIERS: u8 = 0x01;
const GET_FIRMWARE_PARAMETERS: u8 = 0x02;
const REQUEST_UPDATE: u8 = 0x10;
const PASS_COMPONENT_TABLE: u8 = 0x13;
const UPDATE_COMPONENT: u8 = 0x14;
const REQUEST_FIRMWARE_DATA: u8 = 0x15;
const TRANSFER_COMPLETE: u8 = 0x16;
const VERIFY_COMPLETE: u8 = 0x17;
const APPLY_COMPLETE: u8 = 0x18;
const ACTIVATE_FIRMWARE: u8 = 0x1a;

/// The completion code of a command that succeeded.
const SUCCESS: u8 = 0;

/// The descriptor type of a UUID.
const DESCRIPTOR_UUID: u16 = 0x0002;

/// The string type ASCII.
const STRING_ASCII: u8 = 1;

/// The largest part of a component that the update agent sends in one
/// `RequestFirmwareData` response.
const MAX_TRANSFER_SIZE: u32 = 512;

/// The version string and the comparison stamp of the update.
const UPDATE_VERSION: &str = "2.0.0";
const UPDATE_COMPARISON_STAMP: u32 = 2;

/// `PassComponentTable` transfer flags.
const TRANSFER_START: u8 = 0x01;
const TRANSFER_END: u8 = 0x04;

/// Cycles to wait for a log line of the firmware.
const LOG_TIMEOUT_CYCLES: u64 = 50_000_000;

/// Cycles to wait for the updated firmware to start: Caliptra checks the
/// manifest, the MCU is reset, and Caliptra copies and authorizes the
/// firmware.
const UPDATE_TIMEOUT_CYCLES: u64 = 200_000_000;

/// A component of the update.
struct Component<'a> {
    name: &'static str,
    classification: u16,
    identifier: u16,
    image: &'a [u8],
}

fn start(host: Host) -> ScenarioFuture {
    Box::pin(run(host))
}

fn command_name(command: u8) -> &'static str {
    match command {
        QUERY_DEVICE_IDENTIFIERS => "QueryDeviceIdentifiers",
        GET_FIRMWARE_PARAMETERS => "GetFirmwareParameters",
        REQUEST_UPDATE => "RequestUpdate",
        PASS_COMPONENT_TABLE => "PassComponentTable",
        UPDATE_COMPONENT => "UpdateComponent",
        REQUEST_FIRMWARE_DATA => "RequestFirmwareData",
        TRANSFER_COMPLETE => "TransferComplete",
        VERIFY_COMPLETE => "VerifyComplete",
        APPLY_COMPLETE => "ApplyComplete",
        ACTIVATE_FIRMWARE => "ActivateFirmware",
        _ => "unknown command",
    }
}

/// A PLDM version string field: the type, the length and the characters.
fn version_field(version: &str) -> Vec<u8> {
    let mut field = vec![STRING_ASCII, version.len() as u8];
    field.extend_from_slice(version.as_bytes());
    field
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, String> {
    bytes
        .get(offset..offset + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| "the message is too short".to_string())
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| "the message is too short".to_string())
}

fn byte_at(bytes: &[u8], offset: usize) -> Result<u8, String> {
    bytes
        .get(offset)
        .copied()
        .ok_or_else(|| "the message is too short".to_string())
}

/// Fails unless `actual` is `expected`.
fn expect<T: PartialEq + std::fmt::Debug>(
    what: &str,
    actual: T,
    expected: T,
) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{what}: got {actual:?}, expected {expected:?}"))
    }
}

/// A request that the firmware device sent.
struct PldmRequest {
    mctp: DeviceRequest,
    /// The PLDM instance identifier, which the response repeats.
    instance: u8,
    command: u8,
    /// The request after the PLDM header.
    payload: Vec<u8>,
}

/// The update agent.
struct UpdateAgent {
    host: Host,
    mctp: MctpHost,
    /// The PLDM instance identifier of the next request.
    instance: u8,
}

impl UpdateAgent {
    /// Sends the firmware update command `command` with `payload` and
    /// returns the response after its PLDM header: the completion code and
    /// the response data.
    async fn request(&mut self, command: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        let name = command_name(command);
        let instance = self.instance;
        self.instance = (self.instance + 1) & 0x1f;
        let mut request = vec![PLDM_REQUEST | instance, PLDM_TYPE_FW_UPDATE, command];
        request.extend_from_slice(payload);
        self.host.message(Direction::Tx, "PLDM", name, &request);
        let response = self.mctp.request(PLDM_MSG_TYPE, &request).await?;
        self.host.message(
            Direction::Rx,
            "PLDM",
            &format!("{name} response"),
            &response,
        );
        if response.len() < 4 {
            return Err(format!("the {name} response is {} bytes", response.len()));
        }
        expect("response instance identifier", response[0], instance)?;
        expect(
            "response PLDM type",
            response[1] & 0x3f,
            PLDM_TYPE_FW_UPDATE,
        )?;
        expect("response command", response[2], command)?;
        expect(&format!("{name} completion code"), response[3], SUCCESS)?;
        Ok(response[3..].to_vec())
    }

    /// Waits for a request from the firmware device. `show` says whether the
    /// page lists the message.
    async fn device_request(&mut self, show: impl Fn(u8) -> bool) -> Result<PldmRequest, String> {
        let mctp = self.mctp.receive_request().await?;
        expect("message type", mctp.msg_type, PLDM_MSG_TYPE)?;
        let body = &mctp.payload;
        if body.len() < 3 {
            return Err(format!("a request of {} bytes from the device", body.len()));
        }
        expect("request bit", body[0] & PLDM_REQUEST, PLDM_REQUEST)?;
        expect("request PLDM type", body[1] & 0x3f, PLDM_TYPE_FW_UPDATE)?;
        let (instance, command, payload) = (body[0] & 0x1f, body[2], body[3..].to_vec());
        if show(command) {
            self.host
                .message(Direction::Rx, "PLDM", command_name(command), body);
        }
        Ok(PldmRequest {
            mctp,
            instance,
            command,
            payload,
        })
    }

    /// Answers `request` with a completion code of success and `data`.
    async fn respond(
        &mut self,
        request: &PldmRequest,
        data: &[u8],
        show: bool,
    ) -> Result<(), String> {
        let mut response = vec![
            request.instance,
            PLDM_TYPE_FW_UPDATE,
            request.command,
            SUCCESS,
        ];
        response.extend_from_slice(data);
        if show {
            let name = format!("{} response", command_name(request.command));
            self.host.message(Direction::Tx, "PLDM", &name, &response);
        }
        self.mctp.respond(&request.mctp, &response).await
    }

    /// `QueryDeviceIdentifiers`: checks that the device reports its UUID.
    async fn query_device_identifiers(&mut self) -> Result<(), String> {
        let response = self.request(QUERY_DEVICE_IDENTIFIERS, &[]).await?;
        // [completion code][identifiers length (4)][descriptor count]
        // [descriptor type (2)][descriptor length (2)][descriptor data]
        expect("identifiers length", u32_at(&response, 1)?, 4 + 16)?;
        expect("descriptor count", byte_at(&response, 5)?, 1)?;
        expect("descriptor type", u16_at(&response, 6)?, DESCRIPTOR_UUID)?;
        expect("descriptor length", u16_at(&response, 8)?, 16)?;
        let uuid = response.get(10..).unwrap_or_default();
        expect("device UUID", uuid, &DEVICE_UUID[..])?;
        let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
        self.host
            .note(format!("The device identifies itself by the UUID {hex}."));
        Ok(())
    }

    /// `GetFirmwareParameters`: checks the version and the two components
    /// that the device reports.
    async fn get_firmware_parameters(&mut self) -> Result<(), String> {
        let response = self.request(GET_FIRMWARE_PARAMETERS, &[]).await?;
        // [completion code][capabilities (4)][component count (2)]
        // [active string type][active string length]
        // [pending string type][pending string length]
        // [active string][pending string][component table]
        expect("component count", u16_at(&response, 5)?, 2)?;
        let active_len = usize::from(byte_at(&response, 8)?);
        let pending_len = usize::from(byte_at(&response, 10)?);
        let short = || "the GetFirmwareParameters response is too short".to_string();
        let active = response.get(11..11 + active_len).ok_or_else(short)?;
        expect(
            "active image set version",
            active,
            ACTIVE_VERSION.as_bytes(),
        )?;
        expect("pending image set version length", pending_len, 0)?;

        // A component: [classification (2)][identifier (2)]
        // [classification index][active comparison stamp (4)]
        // [active string type][active string length][active release date (8)]
        // [pending comparison stamp (4)][pending string type]
        // [pending string length][pending release date (8)]
        // [activation methods (2)][capabilities (4)]
        // [active string][pending string]
        let mut table = response
            .get(11 + active_len + pending_len..)
            .ok_or_else(short)?;
        let mut components = Vec::new();
        while !table.is_empty() {
            let active_len = usize::from(byte_at(table, 10)?);
            let pending_len = usize::from(byte_at(table, 24)?);
            expect(
                "active comparison stamp",
                u32_at(table, 5)?,
                ACTIVE_COMPARISON_STAMP,
            )?;
            let version = table.get(39..39 + active_len).ok_or_else(short)?;
            expect(
                "active component version",
                version,
                ACTIVE_VERSION.as_bytes(),
            )?;
            components.push((u16_at(table, 0)?, u16_at(table, 2)?));
            table = table
                .get(39 + active_len + pending_len..)
                .ok_or_else(short)?;
        }
        expect(
            "components",
            components,
            vec![
                (MANIFEST_CLASSIFICATION, MANIFEST_IDENTIFIER),
                (RUNTIME_CLASSIFICATION, RUNTIME_IDENTIFIER),
            ],
        )?;
        self.host.note(format!(
            "The device runs version {ACTIVE_VERSION} and has two components: the SoC manifest and the MCU firmware."
        ));
        Ok(())
    }

    /// `RequestUpdate` for an update of `components` components.
    async fn request_update(&mut self, components: u16) -> Result<(), String> {
        let mut payload = MAX_TRANSFER_SIZE.to_le_bytes().to_vec();
        payload.extend_from_slice(&components.to_le_bytes());
        // One outstanding transfer request and no package data.
        payload.push(1);
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&version_field(UPDATE_VERSION));
        let response = self.request(REQUEST_UPDATE, &payload).await?;
        // [completion code][device metadata length (2)]
        // [device will send GetPackageData]
        expect("device metadata length", u16_at(&response, 1)?, 0)?;
        expect("device asks for package data", byte_at(&response, 3)?, 0)?;
        Ok(())
    }

    /// `PassComponentTable` for `component`.
    async fn pass_component(
        &mut self,
        component: &Component<'_>,
        transfer_flag: u8,
    ) -> Result<(), String> {
        let mut payload = vec![transfer_flag];
        payload.extend_from_slice(&component.classification.to_le_bytes());
        payload.extend_from_slice(&component.identifier.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&UPDATE_COMPARISON_STAMP.to_le_bytes());
        payload.extend_from_slice(&version_field(UPDATE_VERSION));
        let response = self.request(PASS_COMPONENT_TABLE, &payload).await?;
        // [completion code][component response][component response code]
        expect(
            &format!("component response for the {}", component.name),
            response.get(1..3),
            Some(&[0, 0][..]),
        )
    }

    /// `UpdateComponent` for `component`, then the requests that the device
    /// sends until it has transferred, verified and applied the component.
    async fn update_component(&mut self, component: &Component<'_>) -> Result<(), String> {
        let total = component.image.len();
        let mut payload = component.classification.to_le_bytes().to_vec();
        payload.extend_from_slice(&component.identifier.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&UPDATE_COMPARISON_STAMP.to_le_bytes());
        payload.extend_from_slice(&(total as u32).to_le_bytes());
        // No update option flags.
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&version_field(UPDATE_VERSION));
        let response = self.request(UPDATE_COMPONENT, &payload).await?;
        // [completion code][compatibility response]
        // [compatibility response code][update option flags enabled (4)]
        // [time before RequestFirmwareData (2)]
        expect(
            &format!("compatibility response for the {}", component.name),
            response.get(1..3),
            Some(&[0, 0][..]),
        )?;

        let mut sent = 0usize;
        let mut transferred = false;
        let mut verified = false;
        self.host.progress(component.name, 0, total);
        loop {
            // The page lists the first RequestFirmwareData exchange of a
            // component and shows the rest as progress.
            let first = sent == 0;
            let request = self
                .device_request(|command| command != REQUEST_FIRMWARE_DATA || first)
                .await?;
            match request.command {
                REQUEST_FIRMWARE_DATA => {
                    // [offset (4)][length (4)]
                    let offset = u32_at(&request.payload, 0)? as usize;
                    let length = u32_at(&request.payload, 4)? as usize;
                    if length > MAX_TRANSFER_SIZE as usize {
                        return Err(format!("the device asked for {length} bytes at once"));
                    }
                    expect("offset of the part the device asks for", offset, sent)?;
                    // A part that ends after the component is padded with
                    // zeros.
                    let mut data = vec![0u8; length];
                    let end = (offset + length).min(total);
                    data[..end - offset].copy_from_slice(&component.image[offset..end]);
                    self.respond(&request, &data, first).await?;
                    sent = end;
                    self.host.progress(component.name, sent, total);
                }
                TRANSFER_COMPLETE => {
                    expect("transfer result", &request.payload[..], &[0][..])?;
                    expect("bytes transferred", sent, total)?;
                    transferred = true;
                    self.respond(&request, &[], true).await?;
                }
                VERIFY_COMPLETE => {
                    if !transferred {
                        return Err("VerifyComplete before TransferComplete".into());
                    }
                    expect("verify result", &request.payload[..], &[0][..])?;
                    verified = true;
                    self.respond(&request, &[], true).await?;
                }
                APPLY_COMPLETE => {
                    // [apply result][activation methods modification (2)]
                    if !verified {
                        return Err("ApplyComplete before VerifyComplete".into());
                    }
                    expect("apply result", byte_at(&request.payload, 0)?, 0)?;
                    self.respond(&request, &[], true).await?;
                    return Ok(());
                }
                other => {
                    return Err(format!("unexpected request 0x{other:02x} from the device"));
                }
            }
        }
    }

    /// `ActivateFirmware`, with self-contained activation.
    async fn activate_firmware(&mut self) -> Result<(), String> {
        let response = self.request(ACTIVATE_FIRMWARE, &[1]).await?;
        // [completion code][estimated time of the activation (2)]
        let seconds = u16_at(&response, 1)?;
        self.host.note(format!(
            "The device accepted the activation and estimates {seconds} seconds for it."
        ));
        Ok(())
    }
}

async fn run(host: Host) -> Result<(), String> {
    let update = host.update_images()?;
    let manifest = Component {
        name: "SoC manifest",
        classification: MANIFEST_CLASSIFICATION,
        identifier: MANIFEST_IDENTIFIER,
        image: &update.soc_manifest,
    };
    let firmware = Component {
        name: "MCU firmware",
        classification: RUNTIME_CLASSIFICATION,
        identifier: RUNTIME_IDENTIFIER,
        image: &update.mcu_fw,
    };

    host.step("Boot Caliptra and the MCU ROM");
    host.wait_for_boot(BOOT_TIMEOUT_CYCLES).await?;

    host.step("Wait for the firmware device");
    host.wait_for_log(READY, LOG_TIMEOUT_CYCLES).await?;
    host.wait_for_log(CONTROL_READY, LOG_TIMEOUT_CYCLES).await?;
    let mut mctp = MctpHost::new(&host, HOST_EID, DEVICE_EID)?;
    mctp.set_packet_body(PACKET_BODY);
    mctp.set_write_gap(WRITE_GAP_CYCLES);
    let mut agent = UpdateAgent {
        host: host.clone(),
        mctp,
        instance: 0,
    };

    host.step("Identify the device");
    agent.query_device_identifiers().await?;
    agent.get_firmware_parameters().await?;

    host.step("Start the update");
    agent.request_update(2).await?;
    agent.pass_component(&manifest, TRANSFER_START).await?;
    agent.pass_component(&firmware, TRANSFER_END).await?;
    host.note(format!(
        "The device can update both components to version {UPDATE_VERSION}."
    ));

    // The device verifies the firmware against the manifest, so the manifest
    // goes first.
    host.step("Update the SoC manifest");
    agent.update_component(&manifest).await?;
    host.note("The device transferred the manifest, and Caliptra verified its signatures.");

    host.step("Update the MCU firmware");
    agent.update_component(&firmware).await?;
    host.note(
        "The device transferred the firmware and checked it against the digest in the manifest.",
    );

    host.step("Activate the update");
    agent.activate_firmware().await?;

    host.step("Run the updated firmware");
    host.note("Caliptra resets the MCU, copies the firmware to MCU SRAM and authorizes it.");
    host.wait_for_log(UPDATED, UPDATE_TIMEOUT_CYCLES).await?;
    match host.wait_for_exit(LOG_TIMEOUT_CYCLES).await? {
        0 => {
            host.note(
                "The updated firmware ran after a hitless update reset and exited with PASS.",
            );
            Ok(())
        }
        status => Err(format!("the updated firmware exited with status {status}")),
    }
}
