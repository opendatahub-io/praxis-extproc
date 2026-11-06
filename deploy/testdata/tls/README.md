# KIND provided-TLS material (optional)

Demo and e2e overlays do **not** use this directory: they run with
`tls.mode: none` / `self_signed` and strip the base Deployment's provided-TLS
volume. ODH/OpenShift uses service-ca (`serving-cert-secret-name` on the ODH
overlay Services only).

Generate PEMs here only when experimenting with `tls.mode: provided` on KIND:

```console
python3 hack/scripts/mint-test-tls.py
```

`tls.crt` / `tls.key` are gitignored. Never commit private keys or Secret
manifests that embed them.
