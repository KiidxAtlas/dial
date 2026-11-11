//! Certificates, certificate requests and public keys (PEM or DER) decoded into their fields.
//! Private-key blocks are identified by type and size only; key material is never echoed.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use x509_parser::{
    certificate::X509Certificate,
    certification_request::X509CertificationRequest,
    extensions::{GeneralName, ParsedExtension},
    objects::{oid_registry, oid2sn},
    oid_registry::Oid,
    pem::Pem,
    prelude::FromDer,
    public_key::PublicKey,
    x509::SubjectPublicKeyInfo,
};

#[cfg(test)]
mod tests;

/// Most PEM blocks decoded from one file.
const MAX_BLOCKS: usize = 256;

pub(super) fn read(bytes: &[u8]) -> Result<String, String> {
    let mut out = String::new();
    if bytes.trim_ascii_start().starts_with(b"-----BEGIN ") {
        let mut count = 0;
        for pem in Pem::iter_from_buffer(bytes) {
            let pem = pem.map_err(|e| format!("invalid PEM: {e}"))?;
            count += 1;
            if count > MAX_BLOCKS {
                return Err(format!("PEM file exceeds {MAX_BLOCKS} blocks"));
            }
            let _ = writeln!(out, "[block {count}: {}]", pem.label);
            out.push_str(&block(&pem.label, &pem.contents));
        }
        if count == 0 {
            return Err("no PEM blocks found".into());
        }
    } else {
        out.push_str("[DER]\n");
        out.push_str(&der(bytes)?);
    }
    Ok(out)
}

fn block(label: &str, contents: &[u8]) -> String {
    let decoded = match label {
        "CERTIFICATE" | "TRUSTED CERTIFICATE" | "X509 CERTIFICATE" => certificate(contents),
        "CERTIFICATE REQUEST" | "NEW CERTIFICATE REQUEST" => request(contents),
        "PUBLIC KEY" => SubjectPublicKeyInfo::from_der(contents)
            .map(|(_, spki)| format!("Public key: {}\n", key(&spki)))
            .map_err(|e| format!("invalid public key: {e}")),
        label if label.contains("PRIVATE KEY") => Ok(format!(
            "Private key ({label}, {} bytes): key material not shown. Compare it with a certificate by its public key, e.g. `openssl pkey -in FILE -pubout`.\n",
            contents.len()
        )),
        _ => Ok(format!(
            "{} bytes of {label} data (not decoded)\n",
            contents.len()
        )),
    };
    decoded.unwrap_or_else(|e| format!("not decoded: {e}\n"))
}

/// DER has no label; try each structure in turn.
fn der(bytes: &[u8]) -> Result<String, String> {
    certificate(bytes)
        .or_else(|_| request(bytes))
        .or_else(|_| {
            SubjectPublicKeyInfo::from_der(bytes)
                .map(|(_, spki)| format!("Public key: {}\n", key(&spki)))
                .map_err(|e| e.to_string())
        })
        .map_err(|_| {
            "DER data is not an X.509 certificate, certificate request or public key".to_owned()
        })
}

fn name(oid: &Oid) -> String {
    oid2sn(oid, oid_registry()).map_or_else(|_| oid.to_id_string(), str::to_owned)
}

fn key(spki: &SubjectPublicKeyInfo) -> String {
    let algorithm = name(&spki.algorithm.algorithm);
    match spki.parsed() {
        Ok(PublicKey::RSA(rsa)) => format!("RSA {} bits", rsa.key_size()),
        Ok(PublicKey::EC(point)) => {
            let curve = spki
                .algorithm
                .parameters
                .as_ref()
                .and_then(|p| p.as_oid().ok())
                .map_or_else(|| "unknown curve".to_owned(), |oid| name(&oid));
            format!("EC {curve} ({} bits)", point.key_size())
        }
        Ok(PublicKey::DSA(_)) => "DSA".to_owned(),
        _ => format!("{algorithm} ({} bytes)", spki.subject_public_key.data.len()),
    }
}

fn hex_colon(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn general_names(names: &[GeneralName]) -> String {
    names
        .iter()
        .map(|name| match name {
            GeneralName::DNSName(dns) => format!("DNS:{dns}"),
            GeneralName::RFC822Name(email) => format!("email:{email}"),
            GeneralName::URI(uri) => format!("URI:{uri}"),
            GeneralName::IPAddress(ip) => match ip.len() {
                4 => format!("IP:{}", std::net::Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])),
                16 => {
                    let octets: [u8; 16] = (*ip).try_into().unwrap_or([0; 16]);
                    format!("IP:{}", std::net::Ipv6Addr::from(octets))
                }
                _ => format!("IP:{}", hex_colon(ip)),
            },
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn certificate(der: &[u8]) -> Result<String, String> {
    let (_, cert) = X509Certificate::from_der(der).map_err(|e| e.to_string())?;
    let mut out = String::new();
    let validity = cert.validity();
    let status = if validity.is_valid() {
        "currently valid"
    } else if validity.time_to_expiration().is_none()
        && validity.not_before.timestamp() > time::OffsetDateTime::now_utc().unix_timestamp()
    {
        "NOT YET VALID"
    } else {
        "EXPIRED"
    };
    let _ = writeln!(out, "Certificate (version {})", cert.version().0 + 1);
    let _ = writeln!(out, "  Subject: {}", cert.subject());
    let _ = writeln!(out, "  Issuer: {}", cert.issuer());
    let _ = writeln!(out, "  Serial: {}", cert.raw_serial_as_string());
    let _ = writeln!(
        out,
        "  Validity: {} to {} ({status})",
        validity.not_before, validity.not_after
    );
    let _ = writeln!(out, "  Public key: {}", key(cert.public_key()));
    let _ = writeln!(
        out,
        "  Signature: {}",
        name(&cert.signature_algorithm.algorithm)
    );
    if cert.subject() == cert.issuer() {
        let verified = match cert.verify_signature(None) {
            Ok(()) => "signature verifies with its own key",
            Err(_) => "signature does NOT verify with its own key",
        };
        let _ = writeln!(out, "  Self-signed: yes ({verified})");
    }
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        let _ = writeln!(
            out,
            "  Subject alternative names: {}",
            general_names(&san.value.general_names)
        );
    }
    if let Ok(Some(bc)) = cert.basic_constraints() {
        let _ = writeln!(
            out,
            "  Basic constraints: CA={}{}{}",
            bc.value.ca,
            bc.value
                .path_len_constraint
                .map_or_else(String::new, |n| format!(", path length {n}")),
            if bc.critical { " (critical)" } else { "" }
        );
    }
    if let Ok(Some(usage)) = cert.key_usage() {
        let _ = writeln!(out, "  Key usage: {}", usage.value);
    }
    if let Ok(Some(eku)) = cert.extended_key_usage() {
        let eku = eku.value;
        let mut uses: Vec<String> = [
            (eku.any, "any"),
            (eku.server_auth, "serverAuth"),
            (eku.client_auth, "clientAuth"),
            (eku.code_signing, "codeSigning"),
            (eku.email_protection, "emailProtection"),
            (eku.time_stamping, "timeStamping"),
            (eku.ocsp_signing, "OCSPSigning"),
        ]
        .into_iter()
        .filter(|(set, _)| *set)
        .map(|(_, label)| label.to_owned())
        .collect();
        uses.extend(eku.other.iter().map(name));
        let _ = writeln!(out, "  Extended key usage: {}", uses.join(", "));
    }
    let _ = writeln!(
        out,
        "  SHA-256 fingerprint: {}",
        hex_colon(&Sha256::digest(der))
    );
    Ok(out)
}

fn request(der: &[u8]) -> Result<String, String> {
    let (_, csr) = X509CertificationRequest::from_der(der).map_err(|e| e.to_string())?;
    let info = &csr.certification_request_info;
    let mut out = String::new();
    let _ = writeln!(out, "Certificate request");
    let _ = writeln!(out, "  Subject: {}", info.subject);
    let _ = writeln!(out, "  Public key: {}", key(&info.subject_pki));
    let _ = writeln!(
        out,
        "  Signature: {} ({})",
        name(&csr.signature_algorithm.algorithm),
        if csr.verify_signature().is_ok() {
            "verifies"
        } else {
            "does NOT verify"
        }
    );
    if let Some(extensions) = csr.requested_extensions() {
        for extension in extensions {
            if let ParsedExtension::SubjectAlternativeName(san) = extension {
                let _ = writeln!(
                    out,
                    "  Requested subject alternative names: {}",
                    general_names(&san.general_names)
                );
            }
        }
    }
    Ok(out)
}
