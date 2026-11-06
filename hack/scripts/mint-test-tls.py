#!/usr/bin/env python3
"""Mint local PEMs for optional provided-TLS experiments on KIND.

Demo and e2e overlays use tls.mode none/self_signed and strip the base
provided-TLS volume, so they do not need this material. OpenShift/ODH uses
service-ca. Keep generated PEMs out of git (see .gitignore); never commit a
private key or a Secret that embeds one.
"""

from __future__ import annotations

import datetime
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "deploy" / "testdata" / "tls"

SANS = [
    "payload-processing.istio-system.svc.cluster.local",
    "payload-processing.praxis-extproc.svc.cluster.local",
    "payload-processing.openshift-ingress.svc.cluster.local",
    "payload-pre-processing.istio-system.svc.cluster.local",
    "payload-pre-processing.openshift-ingress.svc.cluster.local",
]


def main() -> None:
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, SANS[0])])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=1))
        .not_valid_after(now + datetime.timedelta(days=3650))
        .add_extension(
            x509.SubjectAlternativeName([x509.DNSName(s) for s in SANS]),
            critical=False,
        )
        .sign(key, hashes.SHA256())
    )

    crt_pem = cert.public_bytes(serialization.Encoding.PEM)
    key_pem = key.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.TraditionalOpenSSL,
        serialization.NoEncryption(),
    )

    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "tls.crt").write_bytes(crt_pem)
    (OUT / "tls.key").write_bytes(key_pem)
    print(f"wrote {OUT / 'tls.crt'} and {OUT / 'tls.key'} (gitignored; do not commit)")


if __name__ == "__main__":
    main()
