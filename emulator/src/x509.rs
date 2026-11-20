// Licensed under the Apache-2.0 license

//! Describes an X.509 certificate for the page, and checks its signature.
//!
//! [`describe`] turns a DER-encoded certificate into a [`CertificateInfo`]:
//! its names, validity, key and extensions as text. It decodes the extensions
//! that Caliptra's certificates carry, among them the TCG DICE ones
//! (`tcg-dice-TcbInfo`, `tcg-dice-MultiTcbInfo` and `tcg-dice-Ueid`), and
//! gives any other extension in hex.
//!
//! [`signed_by`] checks an ECDSA P-384 signature with the issuer's key.

use p384::ecdsa::signature::Verifier;
use p384::ecdsa::{Signature, VerifyingKey};
use serde::Serialize;
use sha2::{Digest, Sha256};
use x509_cert::der::asn1::ObjectIdentifier;
use x509_cert::der::Decode;
use x509_cert::ext::pkix::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, KeyUsages,
    SubjectKeyIdentifier,
};
use x509_cert::ext::Extension;
use x509_cert::name::Name;
use x509_cert::Certificate;

/// A label and a value, e.g. one attribute of a name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Field {
    pub label: String,
    pub value: String,
}

impl Field {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// The subject's public key.
#[derive(Clone, Debug, Serialize)]
pub struct PublicKey {
    /// The algorithm and curve, e.g. "EC P-384".
    pub algorithm: String,
    /// The key in hex: for an EC key, the uncompressed point.
    pub bytes: String,
}

/// Whether the certificate's signature was checked, and the result.
#[derive(Clone, Debug, Serialize)]
pub struct SignatureCheck {
    /// The signature verifies with the issuer's key.
    pub verified: bool,
    /// The result as a sentence.
    pub text: String,
}

/// One extension of a certificate.
#[derive(Clone, Debug, Serialize)]
pub struct ExtensionInfo {
    /// The extension's name, or its OID if it has none here.
    pub name: String,
    pub oid: String,
    pub critical: bool,
    /// The value as groups of fields. Most extensions have one group;
    /// `tcg-dice-MultiTcbInfo` has one for each TCB.
    pub groups: Vec<Vec<Field>>,
}

/// A certificate as the page shows it.
#[derive(Clone, Debug, Serialize)]
pub struct CertificateInfo {
    /// The attributes of the subject's name, in the certificate's order.
    pub subject: Vec<Field>,
    /// The attributes of the issuer's name.
    pub issuer: Vec<Field>,
    /// The serial number in hex.
    pub serial: String,
    /// The start of the validity period, e.g. "2023-01-01T00:00:00Z".
    pub not_before: String,
    /// The end of the validity period.
    pub not_after: String,
    pub public_key: PublicKey,
    pub signature_algorithm: String,
    pub signature_check: SignatureCheck,
    pub extensions: Vec<ExtensionInfo>,
    /// The SHA-256 hash of the certificate in hex.
    pub sha256: String,
    /// The DER encoding in hex.
    pub der: String,
}

impl CertificateInfo {
    /// The common name of the subject.
    pub fn subject_common_name(&self) -> Option<&str> {
        common_name(&self.subject)
    }

    /// The common name of the issuer.
    pub fn issuer_common_name(&self) -> Option<&str> {
        common_name(&self.issuer)
    }
}

fn common_name(name: &[Field]) -> Option<&str> {
    name.iter()
        .find(|field| field.label == "CN")
        .map(|field| field.value.as_str())
}

const ECDSA_WITH_SHA384: &str = "1.2.840.10045.4.3.3";
const EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
const SECP384R1: &str = "1.3.132.0.34";

const TCG_DICE_TCB_INFO: &str = "2.23.133.5.4.1";
const TCG_DICE_UEID: &str = "2.23.133.5.4.4";
const TCG_DICE_MULTI_TCB_INFO: &str = "2.23.133.5.4.5";

/// Names of the OIDs that appear in Caliptra's certificates.
fn oid_name(oid: &str) -> Option<&'static str> {
    Some(match oid {
        // Attributes of a name.
        "2.5.4.3" => "CN",
        "2.5.4.5" => "serialNumber",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        // Algorithms.
        "1.2.840.10045.4.3.2" => "ECDSA with SHA-256",
        ECDSA_WITH_SHA384 => "ECDSA with SHA-384",
        "1.2.840.10045.4.3.4" => "ECDSA with SHA-512",
        "1.2.840.10045.3.1.7" => "P-256",
        SECP384R1 => "P-384",
        "1.3.132.0.35" => "P-521",
        "2.16.840.1.101.3.4.2.1" => "SHA-256",
        "2.16.840.1.101.3.4.2.2" => "SHA-384",
        "2.16.840.1.101.3.4.2.3" => "SHA-512",
        // Extensions.
        "2.5.29.14" => "Subject key identifier",
        "2.5.29.15" => "Key usage",
        "2.5.29.19" => "Basic constraints",
        "2.5.29.35" => "Authority key identifier",
        "2.5.29.37" => "Extended key usage",
        TCG_DICE_TCB_INFO => "DICE TCB info",
        TCG_DICE_UEID => "DICE UEID",
        TCG_DICE_MULTI_TCB_INFO => "DICE multiple TCB info",
        // TCG DICE key purposes.
        "2.23.133.5.4.100.6" => "tcg-dice-kp-identityInit",
        "2.23.133.5.4.100.7" => "tcg-dice-kp-identityLoc",
        "2.23.133.5.4.100.8" => "tcg-dice-kp-attestInit",
        "2.23.133.5.4.100.9" => "tcg-dice-kp-attestLoc",
        "2.23.133.5.4.100.10" => "tcg-dice-kp-assertInit",
        "2.23.133.5.4.100.11" => "tcg-dice-kp-assertLoc",
        "2.23.133.5.4.100.12" => "tcg-dice-kp-eca",
        _ => return None,
    })
}

/// The name of `oid`, or the OID itself.
fn oid_or_name(oid: &ObjectIdentifier) -> String {
    let oid = oid.to_string();
    oid_name(&oid).map_or(oid, Into::into)
}

/// The name of `oid` followed by the OID, or the OID alone.
fn oid_and_name(oid: &ObjectIdentifier) -> String {
    let oid = oid.to_string();
    match oid_name(&oid) {
        Some(name) => format!("{name} ({oid})"),
        None => oid,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One element of a DER encoding.
struct Tlv<'a> {
    tag: u8,
    value: &'a [u8],
    /// The whole element: the tag, the length and the value.
    raw: &'a [u8],
}

impl<'a> Tlv<'a> {
    /// Reads the element at the start of `input`. Returns it and the bytes
    /// after it.
    fn read(input: &'a [u8]) -> Result<(Self, &'a [u8]), String> {
        let truncated = || "truncated DER element".to_string();
        let (&tag, rest) = input.split_first().ok_or_else(truncated)?;
        if tag & 0x1f == 0x1f {
            return Err("DER tag number over 30".into());
        }
        let (&first, rest) = rest.split_first().ok_or_else(truncated)?;
        let (len, rest) = if first < 0x80 {
            (usize::from(first), rest)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 || rest.len() < count {
                return Err("bad DER length".into());
            }
            let (len_bytes, rest) = rest.split_at(count);
            let len = len_bytes
                .iter()
                .fold(0usize, |len, &b| (len << 8) | usize::from(b));
            (len, rest)
        };
        if rest.len() < len {
            return Err(truncated());
        }
        let (value, rest) = rest.split_at(len);
        let raw = &input[..input.len() - rest.len()];
        Ok((Self { tag, value, raw }, rest))
    }

    /// Reads the one element that `input` consists of.
    fn read_all(input: &'a [u8]) -> Result<Self, String> {
        match Self::read(input)? {
            (tlv, []) => Ok(tlv),
            _ => Err("bytes after the DER element".into()),
        }
    }

    /// The elements that the value of a constructed element consists of.
    fn children(&self) -> Result<Vec<Tlv<'a>>, String> {
        let mut children = Vec::new();
        let mut rest = self.value;
        while !rest.is_empty() {
            let (child, after) = Tlv::read(rest)?;
            children.push(child);
            rest = after;
        }
        Ok(children)
    }

    /// The number of a context-specific tag, primitive or constructed.
    fn context_tag(&self) -> Option<u8> {
        (self.tag & 0xc0 == 0x80).then_some(self.tag & 0x1f)
    }
}

/// The value of an INTEGER as a decimal number, followed by its bytes in hex
/// if there is more than one. An integer of more than 8 bytes is given in hex
/// alone.
fn integer(value: &[u8]) -> String {
    if value.is_empty() || value.len() > 8 {
        return format!("0x{}", hex(value));
    }
    let negative = value[0] & 0x80 != 0;
    let number = value
        .iter()
        .fold(if negative { -1i64 } else { 0 }, |n, &b| {
            (n << 8) | i64::from(b)
        });
    if value.len() == 1 {
        number.to_string()
    } else {
        format!("{number} (0x{})", hex(value))
    }
}

/// `bytes` as text if they are printable ASCII, else in hex after "0x".
fn text_or_hex(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.is_empty() && text.chars().all(|c| c.is_ascii_graphic() || c == ' ') => {
            text.into()
        }
        _ => format!("0x{}", hex(bytes)),
    }
}

/// The attributes of `name`, in the certificate's order.
fn name_fields(name: &Name) -> Vec<Field> {
    name.0
        .iter()
        .flat_map(|rdn| rdn.0.iter())
        .map(|attribute| {
            Field::new(
                oid_or_name(&attribute.oid),
                text_or_hex(attribute.value.value()),
            )
        })
        .collect()
}

/// The fields of a `FWIDLIST`: a digest for each, labelled with `what` and
/// the hash algorithm.
fn fwid_fields(list: &Tlv, what: &str, fields: &mut Vec<Field>) -> Result<(), String> {
    for fwid in list.children()? {
        let [algorithm, digest] = &fwid.children()?[..] else {
            return Err("FWID is not an algorithm and a digest".into());
        };
        let algorithm = ObjectIdentifier::from_bytes(algorithm.value)
            .map_or_else(|_| hex(algorithm.value), |oid| oid_or_name(&oid));
        fields.push(Field::new(
            format!("{what} ({algorithm})"),
            hex(digest.value),
        ));
    }
    Ok(())
}

/// The fields of a `DiceTcbInfo` (TCG DICE Attestation Architecture):
///
/// ```text
/// DiceTcbInfo ::= SEQUENCE {
///     vendor     [0] IMPLICIT UTF8String OPTIONAL,
///     model      [1] IMPLICIT UTF8String OPTIONAL,
///     version    [2] IMPLICIT UTF8String OPTIONAL,
///     svn        [3] IMPLICIT INTEGER OPTIONAL,
///     layer      [4] IMPLICIT INTEGER OPTIONAL,
///     index      [5] IMPLICIT INTEGER OPTIONAL,
///     fwids      [6] IMPLICIT FWIDLIST OPTIONAL,
///     flags      [7] IMPLICIT OperationalFlags OPTIONAL,
///     vendorInfo [8] IMPLICIT OCTET STRING OPTIONAL,
///     type       [9] IMPLICIT OCTET STRING OPTIONAL,
///     flagsMask  [10] IMPLICIT OperationalFlagsMask OPTIONAL,
///     integrityRegisters [11] IMPLICIT IrList OPTIONAL }
/// ```
fn tcb_info_fields(tcb_info: &Tlv) -> Result<Vec<Field>, String> {
    let mut fields = Vec::new();
    for element in tcb_info.children()? {
        let value = element.value;
        // A bit string's first byte counts its unused bits.
        let bits = || format!("0x{}", hex(value.get(1..).unwrap_or_default()));
        match element.context_tag() {
            Some(0) => fields.push(Field::new("Vendor", text_or_hex(value))),
            Some(1) => fields.push(Field::new("Model", text_or_hex(value))),
            Some(2) => fields.push(Field::new("Version", text_or_hex(value))),
            Some(3) => fields.push(Field::new("SVN", integer(value))),
            Some(4) => fields.push(Field::new("Layer", integer(value))),
            Some(5) => fields.push(Field::new("Index", integer(value))),
            Some(6) => fwid_fields(&element, "FWID", &mut fields)?,
            Some(7) => fields.push(Field::new("Flags", bits())),
            Some(8) => fields.push(Field::new("Vendor info", format!("0x{}", hex(value)))),
            Some(9) => fields.push(Field::new("Type", text_or_hex(value))),
            Some(10) => fields.push(Field::new("Flags mask", bits())),
            Some(11) => {
                // IntegrityRegister ::= SEQUENCE {
                //     registerName    [0] IMPLICIT IA5String OPTIONAL,
                //     registerNum     [1] IMPLICIT INTEGER OPTIONAL,
                //     registerDigests [2] IMPLICIT FWIDLIST }
                for register in element.children()? {
                    let mut what = "Integrity register".to_string();
                    for part in register.children()? {
                        match part.context_tag() {
                            Some(0) => what = format!("{what} {}", text_or_hex(part.value)),
                            Some(1) => what = format!("{what} {}", integer(part.value)),
                            Some(2) => fwid_fields(&part, &what, &mut fields)?,
                            _ => {}
                        }
                    }
                }
            }
            _ => fields.push(Field::new(format!("Tag 0x{:02x}", element.tag), hex(value))),
        }
    }
    Ok(fields)
}

fn key_usage_name(usage: KeyUsages) -> &'static str {
    match usage {
        KeyUsages::DigitalSignature => "digitalSignature",
        KeyUsages::NonRepudiation => "nonRepudiation",
        KeyUsages::KeyEncipherment => "keyEncipherment",
        KeyUsages::DataEncipherment => "dataEncipherment",
        KeyUsages::KeyAgreement => "keyAgreement",
        KeyUsages::KeyCertSign => "keyCertSign",
        KeyUsages::CRLSign => "cRLSign",
        KeyUsages::EncipherOnly => "encipherOnly",
        KeyUsages::DecipherOnly => "decipherOnly",
    }
}

/// The value of `extension` as groups of fields.
fn extension_groups(oid: &str, value: &[u8]) -> Result<Vec<Vec<Field>>, String> {
    let der = |e: x509_cert::der::Error| e.to_string();
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let one = |field: Field| vec![vec![field]];
    Ok(match oid {
        "2.5.29.19" => {
            let constraints = BasicConstraints::from_der(value).map_err(der)?;
            let mut fields = vec![Field::new("CA", yes_no(constraints.ca))];
            if let Some(len) = constraints.path_len_constraint {
                fields.push(Field::new("Path length", len.to_string()));
            }
            vec![fields]
        }
        "2.5.29.15" => {
            let usage = KeyUsage::from_der(value).map_err(der)?;
            let names: Vec<&str> = usage.0.into_iter().map(key_usage_name).collect();
            one(Field::new("Usage", names.join(", ")))
        }
        "2.5.29.37" => {
            let purposes = ExtendedKeyUsage::from_der(value).map_err(der)?;
            vec![purposes
                .0
                .iter()
                .map(|oid| Field::new("Purpose", oid_and_name(oid)))
                .collect()]
        }
        "2.5.29.14" => {
            let id = SubjectKeyIdentifier::from_der(value).map_err(der)?;
            one(Field::new("Key identifier", hex(id.0.as_bytes())))
        }
        "2.5.29.35" => {
            let id = AuthorityKeyIdentifier::from_der(value).map_err(der)?;
            match id.key_identifier {
                Some(id) => one(Field::new("Key identifier", hex(id.as_bytes()))),
                None => one(Field::new("Value", hex(value))),
            }
        }
        // TcgUeid ::= SEQUENCE { ueid OCTET STRING }
        TCG_DICE_UEID => {
            let ueid = Tlv::read_all(value)?;
            match &ueid.children()?[..] {
                [id] => one(Field::new("UEID", hex(id.value))),
                _ => return Err("UEID is not one octet string".into()),
            }
        }
        TCG_DICE_TCB_INFO => vec![tcb_info_fields(&Tlv::read_all(value)?)?],
        // DiceTcbInfoSeq ::= SEQUENCE SIZE (1..MAX) OF DiceTcbInfo
        TCG_DICE_MULTI_TCB_INFO => Tlv::read_all(value)?
            .children()?
            .iter()
            .map(tcb_info_fields)
            .collect::<Result<_, _>>()?,
        _ => one(Field::new("Value", hex(value))),
    })
}

fn extension_info(extension: &Extension) -> ExtensionInfo {
    let oid = extension.extn_id.to_string();
    let value = extension.extn_value.as_bytes();
    // An extension that does not decode is shown in hex.
    let groups = extension_groups(&oid, value)
        .unwrap_or_else(|_| vec![vec![Field::new("Value", hex(value))]]);
    ExtensionInfo {
        name: oid_name(&oid).map_or_else(|| oid.clone(), Into::into),
        oid,
        critical: extension.critical,
        groups,
    }
}

fn parse(der: &[u8]) -> Result<Certificate, String> {
    Certificate::from_der(der).map_err(|e| format!("not an X.509 certificate: {e}"))
}

/// Describes the DER-encoded certificate `der`. The signature is reported as
/// not checked.
pub fn describe(der: &[u8]) -> Result<CertificateInfo, String> {
    let certificate = parse(der)?;
    let tbs = &certificate.tbs_certificate;

    let key = &tbs.subject_public_key_info;
    let key_algorithm = key.algorithm.oid.to_string();
    let curve = key
        .algorithm
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.decode_as::<ObjectIdentifier>().ok());
    let algorithm = match (key_algorithm.as_str(), curve) {
        (EC_PUBLIC_KEY, Some(curve)) => format!("EC {}", oid_or_name(&curve)),
        _ => key_algorithm,
    };

    Ok(CertificateInfo {
        subject: name_fields(&tbs.subject),
        issuer: name_fields(&tbs.issuer),
        serial: hex(tbs.serial_number.as_bytes()),
        not_before: tbs.validity.not_before.to_date_time().to_string(),
        not_after: tbs.validity.not_after.to_date_time().to_string(),
        public_key: PublicKey {
            algorithm,
            bytes: hex(key.subject_public_key.raw_bytes()),
        },
        signature_algorithm: oid_or_name(&certificate.signature_algorithm.oid),
        signature_check: SignatureCheck {
            verified: false,
            text: "Not checked.".into(),
        },
        extensions: tbs
            .extensions
            .iter()
            .flatten()
            .map(extension_info)
            .collect(),
        sha256: hex(&Sha256::digest(der)),
        der: hex(der),
    })
}

/// A 48-byte big-endian scalar from the value of a DER INTEGER.
fn scalar(value: &[u8]) -> Result<[u8; 48], String> {
    let digits = match value {
        [0, rest @ ..] => rest,
        digits => digits,
    };
    if digits.len() > 48 {
        return Err("ECDSA signature value over 48 bytes".into());
    }
    let mut scalar = [0u8; 48];
    scalar[48 - digits.len()..].copy_from_slice(digits);
    Ok(scalar)
}

/// Whether the signature of `certificate` verifies with the public key of
/// `issuer`. Both are DER-encoded certificates. Fails unless the signature is
/// ECDSA with SHA-384 and the key is on P-384.
pub fn signed_by(certificate: &[u8], issuer: &[u8]) -> Result<bool, String> {
    let parsed = parse(certificate)?;
    let algorithm = parsed.signature_algorithm.oid.to_string();
    if algorithm != ECDSA_WITH_SHA384 {
        return Err(format!("signature algorithm {algorithm} is not supported"));
    }
    let issuer = parse(issuer)?;
    let key = &issuer.tbs_certificate.subject_public_key_info;
    let key = VerifyingKey::from_sec1_bytes(key.subject_public_key.raw_bytes())
        .map_err(|_| "the issuer's key is not a P-384 key")?;

    // The signed bytes are the first element of the certificate, as encoded.
    let outer = Tlv::read_all(certificate)?;
    let children = outer.children()?;
    let tbs = children.first().ok_or("empty certificate")?;

    // Ecdsa-Sig-Value ::= SEQUENCE { r INTEGER, s INTEGER }
    let signature = Tlv::read_all(parsed.signature.raw_bytes())?;
    let [r, s] = &signature.children()?[..] else {
        return Err("the ECDSA signature is not two integers".into());
    };
    let Ok(signature) = Signature::from_scalars(scalar(r.value)?, scalar(s.value)?) else {
        return Ok(false);
    };
    Ok(key.verify(tbs.raw, &signature).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The chain that OpenPRoT's SPDM responder image serves on the emulator.
    const LDEVID: &[u8] = include_bytes!("../tests/data/chain/ldevid.der");
    const FMC_ALIAS: &[u8] = include_bytes!("../tests/data/chain/fmc_alias.der");
    const RT_ALIAS: &[u8] = include_bytes!("../tests/data/chain/rt_alias.der");
    const LEAF: &[u8] = include_bytes!("../tests/data/chain/leaf.der");

    fn extension<'a>(info: &'a CertificateInfo, oid: &str) -> &'a ExtensionInfo {
        info.extensions
            .iter()
            .find(|extension| extension.oid == oid)
            .unwrap_or_else(|| panic!("no extension {oid}"))
    }

    fn value<'a>(fields: &'a [Field], label: &str) -> &'a str {
        fields
            .iter()
            .find(|field| field.label == label)
            .map(|field| field.value.as_str())
            .unwrap_or_else(|| panic!("no field {label}"))
    }

    #[test]
    fn describes_the_ldevid_certificate() {
        let info = describe(LDEVID).unwrap();
        assert_eq!(
            info.subject_common_name(),
            Some("Caliptra 2.1 Ecc384 LDevID")
        );
        assert_eq!(
            info.issuer_common_name(),
            Some("Caliptra 2.1 Ecc384 IDevID")
        );
        assert_eq!(
            value(&info.subject, "serialNumber"),
            "D96E37E70C4C892C60CD3306797C910B278C052656F067EB9531CDE2AC862D1B"
        );
        assert_eq!(info.serial, "5d6e37e70c4c892c60cd3306797c910b278c0526");
        assert_eq!(info.not_before, "2023-01-01T00:00:00Z");
        assert_eq!(info.not_after, "9999-12-31T23:59:59Z");
        assert_eq!(info.public_key.algorithm, "EC P-384");
        assert!(info.public_key.bytes.starts_with("04cddb0310"));
        assert_eq!(info.public_key.bytes.len(), 2 * 97);
        assert_eq!(info.signature_algorithm, "ECDSA with SHA-384");
        assert_eq!(info.der.len(), 2 * LDEVID.len());

        let constraints = extension(&info, "2.5.29.19");
        assert_eq!(constraints.name, "Basic constraints");
        assert!(constraints.critical);
        assert_eq!(value(&constraints.groups[0], "CA"), "yes");
        assert_eq!(value(&constraints.groups[0], "Path length"), "6");
        assert_eq!(
            value(&extension(&info, "2.5.29.15").groups[0], "Usage"),
            "keyCertSign"
        );
        let purposes = &extension(&info, "2.5.29.37").groups[0];
        assert_eq!(
            purposes[0].value,
            "tcg-dice-kp-identityLoc (2.23.133.5.4.100.7)"
        );
        assert_eq!(purposes[1].value, "tcg-dice-kp-eca (2.23.133.5.4.100.12)");
        assert_eq!(
            value(&extension(&info, "2.5.29.14").groups[0], "Key identifier"),
            "d96e37e70c4c892c60cd3306797c910b278c0526"
        );
        assert_eq!(
            value(&extension(&info, "2.5.29.35").groups[0], "Key identifier"),
            "e8bc88779ce71a57d4e0810993b80b4231a591de"
        );
        assert_eq!(
            value(&extension(&info, TCG_DICE_UEID).groups[0], "UEID"),
            "00".repeat(17)
        );
    }

    #[test]
    fn decodes_tcb_info() {
        let info = describe(RT_ALIAS).unwrap();
        let tcb = &extension(&info, TCG_DICE_TCB_INFO).groups[0];
        // Caliptra writes an SVN n as 0x0100 | n, to give it a fixed width.
        assert_eq!(value(tcb, "SVN"), "256 (0x0100)");
        assert_eq!(value(tcb, "Type"), "CALIPTRA_2_X_RT_FIRMWARE_INFO");
        assert!(value(tcb, "FWID (SHA-384)").starts_with("f061e772bce3b6d7"));
        assert_eq!(value(tcb, "FWID (SHA-384)").len(), 96);
        assert_eq!(
            value(&extension(&info, "2.5.29.15").groups[0], "Usage"),
            "digitalSignature, keyCertSign"
        );
    }

    #[test]
    fn decodes_multiple_tcb_info() {
        let info = describe(FMC_ALIAS).unwrap();
        let multi = extension(&info, TCG_DICE_MULTI_TCB_INFO);
        assert_eq!(multi.name, "DICE multiple TCB info");
        let types: Vec<&str> = multi.groups.iter().map(|tcb| value(tcb, "Type")).collect();
        assert_eq!(
            types,
            [
                "CALIPTRA_2_X_FUSE_OWNER_INFO",
                "CALIPTRA_2_X_FUSE_VENDOR_INFO",
                "CALIPTRA_2_X_FMC_FIRMWARE_INFO"
            ]
        );

        // The leaf has a TCB for each measurement in Caliptra's DPE, each
        // with an integrity register.
        let info = describe(LEAF).unwrap();
        assert_eq!(info.subject_common_name(), Some("DPE Leaf"));
        let multi = extension(&info, TCG_DICE_MULTI_TCB_INFO);
        assert_eq!(value(&multi.groups[0], "Type"), "RMTR");
        assert_eq!(value(&multi.groups[0], "Vendor info"), "0xffffffff");
        assert!(multi.groups[0]
            .iter()
            .any(|field| field.label == "Integrity register 0 (SHA-384)"));
        assert_eq!(value(&extension(&info, "2.5.29.19").groups[0], "CA"), "no");
    }

    #[test]
    fn checks_signatures_along_the_chain() {
        assert!(signed_by(FMC_ALIAS, LDEVID).unwrap());
        assert!(signed_by(RT_ALIAS, FMC_ALIAS).unwrap());
        assert!(signed_by(LEAF, RT_ALIAS).unwrap());
        // The wrong issuer.
        assert!(!signed_by(LEAF, LDEVID).unwrap());
        // A changed certificate.
        let mut changed = RT_ALIAS.to_vec();
        changed[100] ^= 1;
        assert!(!signed_by(&changed, FMC_ALIAS).unwrap_or(false));
    }

    #[test]
    fn rejects_bytes_that_are_no_certificate() {
        assert!(describe(&[]).is_err());
        assert!(describe(&LDEVID[..LDEVID.len() - 1]).is_err());
        assert!(signed_by(&[0x30, 0x00], LDEVID).is_err());
    }

    #[test]
    fn reads_der_elements() {
        let (tlv, rest) = Tlv::read(&[0x04, 0x02, 0xaa, 0xbb, 0xff]).unwrap();
        assert_eq!(
            (tlv.tag, tlv.value, tlv.raw.len(), rest),
            (4, &[0xaa, 0xbb][..], 4, &[0xff][..])
        );
        // A two-byte length.
        let mut long = vec![0x30, 0x82, 0x01, 0x00];
        long.extend_from_slice(&[0; 256]);
        assert_eq!(Tlv::read_all(&long).unwrap().value.len(), 256);
        assert!(Tlv::read(&[0x30, 0x05, 0x00]).is_err());
        assert!(Tlv::read(&[0x30, 0x80]).is_err());
        assert!(Tlv::read(&[0x1f, 0x00]).is_err());
        assert_eq!(integer(&[0x00]), "0");
        assert_eq!(integer(&[0x01, 0x00]), "256 (0x0100)");
        assert_eq!(integer(&[0xff]), "-1");
        assert_eq!(text_or_hex(b"RMTR"), "RMTR");
        assert_eq!(text_or_hex(&[0, 0, 0, 0]), "0x00000000");
    }
}
