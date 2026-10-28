#!/usr/bin/env python3
"""Write an OpenSSH SSHSIG (namespace dial-release) over a file using an Ed25519 key.

The Ed25519 signature is produced by OpenSSL 3 (`pkeyutl -sign -rawin`); this script only
frames it in the SSHSIG format so `ssh-keygen -Y verify` can check it without OpenSSL 3.
"""

import argparse
import base64
import hashlib
import pathlib
import struct
import subprocess
import sys
import tempfile

NAMESPACE = b"dial-release"
# SubjectPublicKeyInfo prefix for Ed25519 (RFC 8410); 32 raw key bytes follow.
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")


def ssh_string(data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + data


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--openssl", required=True, help="OpenSSL 3+ binary")
    parser.add_argument("--key", required=True, help="Ed25519 private key PEM")
    parser.add_argument("--input", required=True, help="file to sign (SHA256SUMS)")
    parser.add_argument("--output", required=True, help="armored SSHSIG output path")
    args = parser.parse_args()

    der = subprocess.run(
        [args.openssl, "pkey", "-in", args.key, "-pubout", "-outform", "DER"],
        check=True,
        capture_output=True,
    ).stdout
    if len(der) != 44 or der[:12] != ED25519_SPKI_PREFIX:
        sys.exit("make-sshsig: key is not an Ed25519 key")
    raw_public = der[12:]

    message = pathlib.Path(args.input).read_bytes()
    signed_data = (
        b"SSHSIG"
        + ssh_string(NAMESPACE)
        + ssh_string(b"")
        + ssh_string(b"sha512")
        + ssh_string(hashlib.sha512(message).digest())
    )
    with tempfile.TemporaryDirectory() as scratch:
        data_path = pathlib.Path(scratch) / "signed-data"
        sig_path = pathlib.Path(scratch) / "signature"
        data_path.write_bytes(signed_data)
        subprocess.run(
            [args.openssl, "pkeyutl", "-sign", "-inkey", args.key, "-rawin",
             "-in", str(data_path), "-out", str(sig_path)],
            check=True,
        )
        raw_signature = sig_path.read_bytes()
    if len(raw_signature) != 64:
        sys.exit("make-sshsig: unexpected Ed25519 signature length")

    public_blob = ssh_string(b"ssh-ed25519") + ssh_string(raw_public)
    signature_blob = ssh_string(b"ssh-ed25519") + ssh_string(raw_signature)
    blob = (
        b"SSHSIG"
        + struct.pack(">I", 1)
        + ssh_string(public_blob)
        + ssh_string(NAMESPACE)
        + ssh_string(b"")
        + ssh_string(b"sha512")
        + ssh_string(signature_blob)
    )
    encoded = base64.b64encode(blob).decode()
    lines = [encoded[i:i + 70] for i in range(0, len(encoded), 70)]
    armored = "-----BEGIN SSH SIGNATURE-----\n" + "\n".join(lines) + "\n-----END SSH SIGNATURE-----\n"
    pathlib.Path(args.output).write_text(armored)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
