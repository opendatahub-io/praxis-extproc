# KIND / demo TLS material

Long-lived self-signed cert used to mint Secret `payload-processing-tls` for
local overlays that inherit the base Deployment TLS volume. OpenShift
service-ca does not run on KIND, so e2e/demo ship
`tls-secret.yaml` built from these files.

Regenerate PEMs and overlay Secrets (needs the `cryptography` Python package):

```console
python3 hack/scripts/mint-test-tls.py
```
