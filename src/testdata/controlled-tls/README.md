# Controlled TLS fixtures

These DER assets contain a test-only ECDSA P-256 CA certificate, a localhost
server certificate and its PKCS#8 private key. They are deliberately public test
material and must never be used by applications. The server certificate has only
localhost DNS identity and ServerAuth usage; validity is 2026-01-01 through
2036-01-01. Tests must regenerate fixtures if that validity window expires.

They were generated using .NET certificate request APIs without changing machine
trust. Production controlled login uses bundled Mozilla roots; only the private
fixture helper trusts ca.der. Tests prove chain/hostname verification, rejection
of this root by the production verifier, bounded cancellation and segmented reads.
All peers are loopback-only and use no broker accounts or credentials.
