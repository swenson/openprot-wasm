// Licensed under the Apache-2.0 license

//! Driver for OpenPRoT's VeeR SPDM responder image
//! (`//target/veer/tests/spdm:spdm_image`): the SPDM requester.
//!
//! It follows `target/veer/tests/spdm/host_test.rs` in OpenPRoT. Each SPDM
//! request goes to the device as an MCTP message over I3C. The driver
//! negotiates a connection (`GET_VERSION`, `GET_CAPABILITIES`,
//! `NEGOTIATE_ALGORITHMS`) and checks each response. It then reads the digest
//! of the certificate chain of slot 0 with `GET_DIGESTS` and the chain itself
//! with `GET_CERTIFICATE`. It checks that the chain holds four certificates,
//! that its root hash is the hash of the first one, and that the digest is
//! the hash of the chain.
//!
//! The driver also reports each certificate to the page, and checks that
//! each one but the first is issued and signed by the one before it. The
//! host test does not check that. The first certificate is issued by
//! Caliptra's IDevID, which has no certificate in the chain, so its
//! signature is not checked.
//!
//! Last, it sends two requests that the responder answers with an error:
//! `GET_CERTIFICATE` for a slot that has no chain, and `CHALLENGE`, which the
//! responder does not support. It checks that the responder still answers
//! `GET_VERSION`.
//!
//! The requests are written out byte by byte and the response fields are read
//! at fixed offsets, as in the host test.

use sha2::{Digest, Sha384};

use super::{Driver, ScenarioFuture, BOOT_TIMEOUT_CYCLES};
use crate::host::{Direction, Host};
use crate::mctp::MctpHost;
use crate::x509::{self, SignatureCheck};

pub const DRIVER: Driver = Driver {
    name: "spdm",
    description: "SPDM requester over MCTP over I3C: negotiates a connection with the responder and reads its certificate chain.",
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
const GET_CERTIFICATE: u8 = 0x82;
const CHALLENGE: u8 = 0x83;
const GET_VERSION: u8 = 0x84;
const GET_CAPABILITIES: u8 = 0xe1;
const NEGOTIATE_ALGORITHMS: u8 = 0xe3;
const DIGESTS: u8 = 0x01;
const CERTIFICATE: u8 = 0x02;
const VERSION: u8 = 0x04;
const CAPABILITIES: u8 = 0x61;
const ALGORITHMS: u8 = 0x63;
const ERROR: u8 = 0x7f;

/// The SPDM error code `InvalidRequest`.
const INVALID_REQUEST: u8 = 0x01;

/// The SPDM error code `UnsupportedRequest`.
const UNSUPPORTED_REQUEST: u8 = 0x07;

/// The `CERT_CAP` responder capability flag.
const CERT_CAP: u32 = 1 << 1;

/// Size in bytes of a SHA-384 hash.
const HASH_LEN: usize = 48;

/// Size in bytes of the header of an SPDM certificate chain: the length of
/// the chain (2 bytes) and a reserved field (2 bytes). The root hash follows
/// the header.
const CHAIN_HEADER_LEN: usize = 4;

/// The number of bytes of the chain that one `GET_CERTIFICATE` request asks
/// for.
const CERTIFICATE_PORTION: u16 = 500;

/// The certificates of the chain, in order: the LDevID, FMC alias and runtime
/// alias certificates of Caliptra, and the leaf certificate.
const CHAIN_CERTIFICATES: [&str; 4] = ["LDevID", "FMC alias", "runtime alias", "leaf"];

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
        GET_CERTIFICATE => "GET_CERTIFICATE".into(),
        CHALLENGE => "CHALLENGE".into(),
        GET_VERSION => "GET_VERSION".into(),
        GET_CAPABILITIES => "GET_CAPABILITIES".into(),
        NEGOTIATE_ALGORITHMS => "NEGOTIATE_ALGORITHMS".into(),
        DIGESTS => "DIGESTS".into(),
        CERTIFICATE => "CERTIFICATE".into(),
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

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Splits `der`, a concatenation of DER-encoded certificates, into the
/// certificates. Each must be a SEQUENCE with a two-byte length.
fn certificates(mut der: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut certificates = Vec::new();
    while !der.is_empty() {
        if der.len() < 4 || der[..2] != [0x30, 0x82] {
            return Err(format!(
                "certificate {} does not start with a SEQUENCE and a two-byte length",
                certificates.len()
            ));
        }
        let len = 4 + usize::from(u16::from_be_bytes([der[2], der[3]]));
        if len > der.len() {
            return Err(format!(
                "certificate {} ends after the chain",
                certificates.len()
            ));
        }
        let (certificate, rest) = der.split_at(len);
        certificates.push(certificate);
        der = rest;
    }
    Ok(certificates)
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

    /// Reads the certificate chain of `slot` with `GET_CERTIFICATE` requests
    /// and returns it in the SPDM format: the header, the root hash and the
    /// certificates.
    async fn get_certificate_chain(&mut self, slot: u8) -> Result<Vec<u8>, String> {
        let mut chain = Vec::new();
        loop {
            // [version][code][slot][param2][Offset x2][Length x2]
            let offset = u16::try_from(chain.len())
                .map_err(|_| "the certificate chain is longer than 65535 bytes")?;
            let mut request = vec![SPDM_12, GET_CERTIFICATE, slot, 0];
            request.extend_from_slice(&offset.to_le_bytes());
            request.extend_from_slice(&CERTIFICATE_PORTION.to_le_bytes());
            let response = self.exchange(&request, [SPDM_12, CERTIFICATE], 8).await?;
            // [version][code][slot][param2][PortionLength x2]
            // [RemainderLength x2][portion...]
            expect_eq("CERTIFICATE slot", response[2], slot)?;
            let portion = usize::from(u16_at(&response, 4));
            let remainder = u16_at(&response, 6);
            expect_eq("CERTIFICATE length", response.len(), 8 + portion)?;
            if portion == 0 {
                return Err("the CERTIFICATE portion is empty".into());
            }
            if portion != usize::from(CERTIFICATE_PORTION) && remainder != 0 {
                return Err("the CERTIFICATE portion is short before the end of the chain".into());
            }
            chain.extend_from_slice(&response[8..]);
            if remainder == 0 {
                return Ok(chain);
            }
        }
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
    expect_eq("responder capability flags", u32_at(&response, 8), CERT_CAP)?;
    expect_eq("DataTransferSize", u32_at(&response, 12), MAX_SPDM_MESSAGE)?;
    expect_eq("MaxSPDMmsgSize", u32_at(&response, 16), MAX_SPDM_MESSAGE)?;
    host.note(format!(
        "The one capability flag is CERT_CAP; messages of up to {MAX_SPDM_MESSAGE} bytes."
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

    // DIGESTS: [version][code][param1][param2 = slot mask][digest per slot]
    host.step("GET_DIGESTS");
    let response = requester
        .exchange(&[SPDM_12, GET_DIGESTS, 0, 0], [SPDM_12, DIGESTS], 4)
        .await?;
    expect_eq("DIGESTS slot mask", response[3], 0x01)?;
    expect_eq("DIGESTS length", response.len(), 4 + HASH_LEN)?;
    let digest = response[4..].to_vec();
    host.note("Slot 0 is the only slot with a certificate chain.");

    // The chain: [Length x2][reserved x2][root hash][certificates...]
    host.step("GET_CERTIFICATE");
    let chain = requester.get_certificate_chain(0).await?;
    if chain.len() < CHAIN_HEADER_LEN + HASH_LEN {
        return Err(format!("certificate chain of {} bytes", chain.len()));
    }
    expect_eq(
        "length in the chain header",
        usize::from(u16_at(&chain, 0)),
        chain.len(),
    )?;
    expect_eq("reserved field of the chain header", u16_at(&chain, 2), 0)?;
    host.note(format!(
        "Slot 0 has a certificate chain of {} bytes.",
        chain.len()
    ));

    host.step("Check the certificate chain");
    let (root_hash, der) = chain[CHAIN_HEADER_LEN..].split_at(HASH_LEN);
    let certificates = certificates(der)?;
    expect_eq(
        "certificates in the chain",
        certificates.len(),
        CHAIN_CERTIFICATES.len(),
    )?;
    if root_hash != Sha384::digest(certificates[0]).as_slice() {
        return Err("the root hash is not the hash of the first certificate".into());
    }
    host.note("The root hash is the SHA-384 hash of the first certificate.");
    if digest != Sha384::digest(&chain).as_slice() {
        return Err("the digest is not the hash of the chain".into());
    }
    host.note("The digest from GET_DIGESTS is the SHA-384 hash of the chain.");

    // Each certificate but the first is issued by the one before it.
    let mut issuer: Option<(&str, &[u8], x509::CertificateInfo)> = None;
    for (name, der) in CHAIN_CERTIFICATES.iter().zip(&certificates) {
        let mut info = x509::describe(der).map_err(|e| format!("{name} certificate: {e}"))?;
        info.signature_check = match &issuer {
            Some((issuer_name, issuer_der, issuer_info)) => {
                if info.issuer != issuer_info.subject {
                    return Err(format!(
                        "the issuer of the {name} certificate is not the subject of the {issuer_name} certificate"
                    ));
                }
                if !x509::signed_by(der, issuer_der)? {
                    return Err(format!(
                        "the signature of the {name} certificate does not verify with the key of the {issuer_name} certificate"
                    ));
                }
                SignatureCheck {
                    verified: true,
                    text: format!("Verifies with the key of the {issuer_name} certificate."),
                }
            }
            None => {
                host.note(format!(
                    "The chain starts at the {name} certificate. Its issuer, {}, has no certificate in the chain.",
                    info.issuer_common_name().unwrap_or("which has no common name")
                ));
                SignatureCheck {
                    verified: false,
                    text: "Not checked: the issuer has no certificate in the chain.".into(),
                }
            }
        };
        host.certificate(name, info.clone());
        issuer = Some((name, der, info));
    }
    host.note("Each certificate after the first is signed by the one before it.");

    // Slot 1 has no chain.
    host.step("GET_CERTIFICATE for an empty slot");
    let response = requester
        .exchange(
            &[SPDM_12, GET_CERTIFICATE, 1, 0, 0, 0, 0xff, 0xff],
            [SPDM_12, ERROR],
            4,
        )
        .await?;
    expect_eq("error code", response[2], INVALID_REQUEST)?;
    host.note("The responder answered InvalidRequest.");

    // The responder does not sign, so it does not support CHALLENGE:
    // [version][code][slot][measurement summary hash type][nonce x32]
    host.step("CHALLENGE");
    let mut request = vec![SPDM_12, CHALLENGE, 0, 0];
    request.extend_from_slice(&[0x5a; 32]);
    let response = requester.exchange(&request, [SPDM_12, ERROR], 4).await?;
    expect_eq("error code", response[2], UNSUPPORTED_REQUEST)?;
    host.note("The responder answered UnsupportedRequest.");

    host.step("GET_VERSION after the errors");
    let versions = requester.get_version().await?;
    if !versions.contains(&SPDM_12) {
        return Err("the responder does not list SPDM 1.2".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::certificates;

    #[test]
    fn splits_concatenated_certificates() {
        let mut der = vec![0x30, 0x82, 0x00, 0x02, 0xaa, 0xbb];
        der.extend_from_slice(&[0x30, 0x82, 0x01, 0x00]);
        der.extend_from_slice(&[0; 256]);
        let split = certificates(&der).unwrap();
        assert_eq!(split.len(), 2);
        assert_eq!(split[0], [0x30, 0x82, 0x00, 0x02, 0xaa, 0xbb]);
        assert_eq!(split[1].len(), 260);
        assert!(certificates(&[]).unwrap().is_empty());
    }

    #[test]
    fn rejects_a_malformed_chain() {
        // Not a SEQUENCE with a two-byte length.
        assert!(certificates(&[0x30, 0x81, 0x01, 0x00]).is_err());
        // The length runs past the end.
        assert!(certificates(&[0x30, 0x82, 0x00, 0x05, 0x00]).is_err());
        // Too short for a header.
        assert!(certificates(&[0x30, 0x82]).is_err());
    }
}
