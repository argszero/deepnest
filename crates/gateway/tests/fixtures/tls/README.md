# Throwaway TLS identities, for the transport tests

These four files are test fixtures and nothing else. No key here protects anything: they were
minted for `crates/gateway/tests/tls.rs`, they are committed on purpose, and the only server that
ever presents them is a listener the test starts on `127.0.0.1` for the length of one handshake.

- `loopback.pem` / `loopback.key` — self-signed, `CN=deepnest-test-loopback`, SAN `IP:127.0.0.1`,
  `extendedKeyUsage=serverAuth`. The name matches the address a test dials, so this is the identity
  a test trusts as a root and then expects to be accepted.
- `other-name.pem` / `other-name.key` — self-signed, `CN=wrong.example`, SAN `DNS:wrong.example`.
  Also trusted as a root, so the only thing wrong with it is that it is for another name: that is
  what makes the mismatch path reachable without a second machine or a public host.

Both are valid until 2029-01-08. macOS refuses a trusted certificate whose validity period is
longer than 825 days, which is why they are not valid for a century.

To mint a replacement, from this directory:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 800 \
  -keyout loopback.key -out loopback.pem -subj "/CN=deepnest-test-loopback" \
  -addext "subjectAltName=IP:127.0.0.1" -addext "extendedKeyUsage=serverAuth"
```

`extendedKeyUsage=serverAuth` is not optional on macOS: without it the platform refuses the
certificate with "The extended key usage is not valid", which looks like a test failure and is not.
