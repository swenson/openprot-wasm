// Licensed under the Apache-2.0 license

//! Driver for OpenPRoT's VeeR SPDM responder image
//! (`//target/veer/tests/spdm:spdm_image`): the SPDM requester.
//!
//! It follows `target/veer/tests/spdm/host_test.rs` in OpenPRoT. Each SPDM
//! request goes to the device as an MCTP message over I3C. The driver
//! negotiates a connection (`GET_VERSION`, `GET_CAPABILITIES`,
//! `NEGOTIATE_ALGORITHMS`) and checks each response. It then sends
//! `GET_DIGESTS`, which the responder answers with an error because it has no
//! certificates, and checks that the responder still answers `GET_VERSION`.
//!
//! The requests are written out byte by byte and the response fields are read
//! at fixed offsets, as in the host test.

use super::{Driver, ScenarioFuture, BOOT_TIMEOUT_CYCLES};
use crate::host::{Direction, Host};
use crate::mctp::MctpHost;

pub const DRIVER: Driver = Driver {
    name: "spdm",
    description: "SPDM requester over MCTP over I3C: negotiates a connection with the responder.",
    start,
};

/// The device's EID and the host's (`OWN_EID` and `HOST_EID` in OpenPRoT's
/// `target/veer/tests/mctp_i3c/ids.rs`).
const DEVICE_EID: u8 = 8;
const HOST_EID: u8 = 9;

/// The MCTP message type of SPDM (DSP0239).
const SPDM_MSG_TYPE: u8 = 5;

/// Logged by the firmware's `spdm_responder.rs` once its listener is armed.
const READY: &str = "spdm responder: waiting for request";

/// SPDM version 1.0, which `GET_VERSION` and `VERSION` always use.
const SPDM_10: u8 = 0x10;
const SPDM_11: u8 = 0x11;
/// SPDM version 1.2, the highest that the responder supports.
const SPDM_12: u8 = 0x12;

// Request and response codes (DSP0274).
const GET_DIGESTS: u8 = 0x81;
const GET_VERSION: u8 = 0x84;
const GET_CAPABILITIES: u8 = 0xe1;
const NEGOTIATE_ALGORITHMS: u8 = 0xe3;
const VERSION: u8 = 0x04;
const CAPABILITIES: u8 = 0x61;
const ALGORITHMS: u8 = 0x63;
const ERROR: u8 = 0x7f;

/// The SPDM error code `UnsupportedRequest`.
const UNSUPPORTED_REQUEST: u8 = 0x07;

/// The largest SPDM message that the responder (`MAX_SPDM_MESSAGE` in its
/// `spdm_responder.rs`) and this requester send or receive.
const MAX_SPDM_MESSAGE: u32 = 1024;

/// `BaseAsymAlgo` bit for ECDSA with NIST P-384.
const ECDSA_P384: u32 = 1 << 7;

/// `BaseHashAlgo` bit for SHA-384.
const SHA_384: u32 = 1 << 1;

fn start(host: Host) -> ScenarioFuture {
    Box::pin(run(host))
}

fn code_name(code: u8) -> String {
    match code {
        GET_DIGESTS => "GET_DIGESTS".into(),
        GET_VERSION => "GET_VERSION".into(),
        GET_CAPABILITIES => "GET_CAPABILITIES".into(),
        NEGOTIATE_ALGORITHMS => "NEGOTIATE_ALGORITHMS".into(),
        VERSION => "VERSION".into(),
        CAPABILITIES => "CAPABILITIES".into(),
        ALGORITHMS => "ALGORITHMS".into(),
        ERROR => "ERROR".into(),
        other => format!("code 0x{other:02x}"),
    }
}

/// Fails with `what` and the two values unless they are equal.
fn expect_eq<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:x?}, expected {want:x?}"))
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

struct Requester {
    host: Host,
    mctp: MctpHost,
}

impl Requester {
    /// Sends one SPDM request and returns the response. Fails unless the
    /// response has the version and code in `want` and at least `min_len`
    /// bytes.
    async fn exchange(
        &mut self,
        request: &[u8],
        want: [u8; 2],
        min_len: usize,
    ) -> Result<Vec<u8>, String> {
        self.host
            .message(Direction::Tx, "SPDM", &code_name(request[1]), request);
        let response = self.mctp.request(SPDM_MSG_TYPE, request).await?;
        let name = response
            .get(1)
            .map_or("empty".into(), |&code| code_name(code));
        self.host.message(Direction::Rx, "SPDM", &name, &response);
        if response.len() < min_len.max(2) {
            return Err(format!(
                "{} response of {} bytes, expected at least {min_len}",
                code_name(want[1]),
                response.len()
            ));
        }
        if response[..2] != want {
            return Err(format!(
                "expected {} with version 0x{:02x}, got {name} with version 0x{:02x}",
                code_name(want[1]),
                want[0],
                response[0]
            ));
        }
        Ok(response)
    }

    /// Sends `GET_VERSION` and returns the versions that the responder
    /// supports, each as one byte (major in the high four bits, minor in the
    /// low four).
    async fn get_version(&mut self) -> Result<Vec<u8>, String> {
        let response = self
            .exchange(&[SPDM_10, GET_VERSION, 0, 0], [SPDM_10, VERSION], 6)
            .await?;
        // [version][code][param1][param2][reserved][count][entries...]
        let count = usize::from(response[5]);
        expect_eq("VERSION length", response.len(), 6 + 2 * count)?;
        // Each entry is 16 bits, little-endian. The high byte has the major
        // version in its high four bits and the minor in its low four.
        let versions: Vec<u8> = response[6..].chunks(2).map(|entry| entry[1]).collect();
        let list: Vec<String> = versions
            .iter()
            .map(|v| format!("{}.{}", v >> 4, v & 0x0f))
            .collect();
        self.host
            .note(format!("The responder supports SPDM {}.", list.join(", ")));
        Ok(versions)
    }
}

async fn run(host: Host) -> Result<(), String> {
    host.step("Boot Caliptra and the MCU ROM");
    host.wait_for_boot(BOOT_TIMEOUT_CYCLES).await?;

    // The responder's listener must be armed before the request arrives.
    host.step("Start the SPDM responder");
    host.wait_for_log(READY, BOOT_TIMEOUT_CYCLES).await?;
    let mut requester = Requester {
        mctp: MctpHost::new(&host, HOST_EID, DEVICE_EID)?,
        host: host.clone(),
    };

    host.step("GET_VERSION");
    let versions = requester.get_version().await?;
    for version in [SPDM_11, SPDM_12] {
        if !versions.contains(&version) {
            return Err(format!(
                "the responder does not list SPDM {}.{}",
                version >> 4,
                version & 0x0f
            ));
        }
    }

    // GET_CAPABILITIES of SPDM 1.2:
    // [version][code][param1][param2][reserved][CTExponent][reserved x2]
    // [Flags x4][DataTransferSize x4][MaxSPDMmsgSize x4]
    host.step("GET_CAPABILITIES");
    let mut request = vec![SPDM_12, GET_CAPABILITIES, 0, 0, 0, 0, 0, 0];
    request.extend_from_slice(&0u32.to_le_bytes());
    request.extend_from_slice(&MAX_SPDM_MESSAGE.to_le_bytes());
    request.extend_from_slice(&MAX_SPDM_MESSAGE.to_le_bytes());
    let response = requester
        .exchange(&request, [SPDM_12, CAPABILITIES], 20)
        .await?;
    expect_eq("CAPABILITIES length", response.len(), 20)?;
    expect_eq("responder capability flags", u32_at(&response, 8), 0)?;
    expect_eq("DataTransferSize", u32_at(&response, 12), MAX_SPDM_MESSAGE)?;
    expect_eq("MaxSPDMmsgSize", u32_at(&response, 16), MAX_SPDM_MESSAGE)?;
    host.note(format!(
        "No capability flags; messages of up to {MAX_SPDM_MESSAGE} bytes."
    ));

    // NEGOTIATE_ALGORITHMS of SPDM 1.2 with no algorithm structures:
    // [version][code][param1 = structures][param2][Length x2]
    // [MeasurementSpecification][OtherParamsSupport][BaseAsymAlgo x4]
    // [BaseHashAlgo x4][reserved x12][ExtAsymCount][ExtHashCount][reserved x2]
    host.step("NEGOTIATE_ALGORITHMS");
    let mut request = vec![SPDM_12, NEGOTIATE_ALGORITHMS, 0, 0, 32, 0, 0x01, 0];
    request.extend_from_slice(&ECDSA_P384.to_le_bytes());
    request.extend_from_slice(&SHA_384.to_le_bytes());
    request.extend_from_slice(&[0; 16]);
    let response = requester
        .exchange(&request, [SPDM_12, ALGORITHMS], 20)
        .await?;
    // [version][code][param1][param2][Length x2][MeasurementSpecificationSel]
    // [OtherParamsSelection][MeasurementHashAlgo x4][BaseAsymSel x4]
    // [BaseHashSel x4]...
    expect_eq(
        "ALGORITHMS length",
        usize::from(u16::from_le_bytes([response[4], response[5]])),
        response.len(),
    )?;
    // The responder has no measurements, so it selects no measurement
    // specification and no measurement hash.
    expect_eq("MeasurementSpecificationSel", response[6], 0)?;
    expect_eq("MeasurementHashAlgo", u32_at(&response, 8), 0)?;
    expect_eq("BaseAsymSel", u32_at(&response, 12), ECDSA_P384)?;
    expect_eq("BaseHashSel", u32_at(&response, 16), SHA_384)?;
    host.note("The responder selected ECDSA P-384 and SHA-384.");

    // The responder has no certificates, so it answers GET_DIGESTS with an
    // error.
    host.step("GET_DIGESTS without certificates");
    let response = requester
        .exchange(&[SPDM_12, GET_DIGESTS, 0, 0], [SPDM_12, ERROR], 4)
        .await?;
    expect_eq("error code", response[2], UNSUPPORTED_REQUEST)?;
    host.note("The responder answered UnsupportedRequest.");

    host.step("GET_VERSION after the error");
    let versions = requester.get_version().await?;
    if !versions.contains(&SPDM_12) {
        return Err("the responder does not list SPDM 1.2".into());
    }
    Ok(())
}
