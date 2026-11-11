use super::*;

const PEM: &[u8] = include_bytes!("tests/fixtures/c.pem");
const DER: &[u8] = include_bytes!("tests/fixtures/c.der");
const CSR: &[u8] = include_bytes!("tests/fixtures/r.csr");
const PUBLIC: &[u8] = include_bytes!("tests/fixtures/pub.pem");
const FINGERPRINT: &str = "FC:72:36:63:FB:D7:0B:05:6A:B4:02:77:B1:1F:0B:18:B2:0E:4B:B6:99:F4:AB:E0:CE:D0:D2:2D:EB:DD:B2:0B";

#[test]
fn pem_and_der_certificates_decode_to_the_same_fields() {
    for bytes in [PEM, DER] {
        let out = read(bytes).unwrap();
        assert!(out.contains("DNS:dial.test, IP:127.0.0.1"), "{out}");
        assert!(out.contains("Subject: CN=dial.test, O=Dial"), "{out}");
        assert!(out.contains("EC prime256v1 (256 bits)"), "{out}");
        assert!(
            out.contains("Self-signed: yes (signature verifies"),
            "{out}"
        );
        assert!(out.contains("(currently valid)"), "{out}");
        assert!(out.contains(FINGERPRINT), "{out}");
    }
}

#[test]
fn requests_and_public_keys_decode() {
    let csr = read(CSR).unwrap();
    assert!(
        csr.contains("Certificate request") && csr.contains("CN=req.test"),
        "{csr}"
    );
    assert!(csr.contains("(verifies)"), "{csr}");
    assert!(read(PUBLIC).unwrap().contains("Public key: EC prime256v1"));
}
#[test]
fn private_key_material_is_never_echoed() {
    use base64::Engine as _;
    let body =
        base64::engine::general_purpose::STANDARD.encode(b"0123456789 fake private key bytes");
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----\n");
    let mut bundle = PEM.to_vec();
    bundle.extend_from_slice(pem.as_bytes());
    let out = read(&bundle).unwrap();
    assert!(out.contains("[block 2: PRIVATE KEY]"), "{out}");
    assert!(!out.contains(&body[..16]), "{out}");
    assert!(!out.contains("SECRET"), "{out}");
}

#[test]
fn unrelated_der_is_refused() {
    assert!(read(&[0x30, 0x03, 0x02, 0x01, 0x05]).is_err());
}
