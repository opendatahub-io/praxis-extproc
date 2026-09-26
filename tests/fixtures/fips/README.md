# FIPS test fixtures

Deliberately weak TLS material for the FIPS behavior tests
(`tests/fips/`). Nothing here is a secret; none of it can serve outside a
test.

| File | What it is | Why |
|---|---|---|
| `rsa1536-cert.pem`, `rsa1536-key.pem` | A self-signed certificate on a 1536-bit RSA key | Below the 2048-bit floor, so the listener must refuse to serve with it in every mode: the validated module enforces it in approved mode, and the platform OpenSSL builds this product supports enforce it at security level 2 everywhere else. |

Regenerate (on a host without FIPS restrictions, which is the point):

```console
openssl req -x509 -newkey rsa:1536 -keyout rsa1536-key.pem \
  -out rsa1536-cert.pem -days 3650 -nodes -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```
