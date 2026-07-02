# Security Policy

## Supported versions
ourios is pre-1.0 and pre-release. Only the `main` branch receives fixes.

## Verifying releases
Every release artifact is signed. Verify before you run anything.

**Container image** — signed keyless with [cosign] (Sigstore; the signature is
recorded in the Rekor transparency log, there is no long-lived key). The image
tag drops the leading `v` (a `v0.1.0` tag publishes `:0.1.0`):

```sh
cosign verify \
  --certificate-identity-regexp '^https://github.com/jensholdgaard/ourios/[.]github/workflows/image[.]yml@refs/tags/v' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/jensholdgaard/ourios:0.1.0
```

**Release artifacts** — the binary archives, the CycloneDX SBOMs (`*.cdx.xml`),
the installer, the source tarball, and the checksums all carry [SLSA build
provenance] attestations. Download an asset from the release and verify it with
the GitHub CLI (the `--repo` bounds the accepted signer identity to this repo's
release workflow):

```sh
gh release download v0.1.0 --pattern 'ourios-server-*-linux-gnu.tar.xz'
gh attestation verify ourios-server-x86_64-unknown-linux-gnu.tar.xz \
  --repo jensholdgaard/ourios
```

The same `gh attestation verify` works on any release asset (e.g. an
`*.cdx.xml` SBOM). Offline, `cosign verify-blob-attestation` against a
downloaded bundle works too.

[cosign]: https://docs.sigstore.dev/cosign/verifying/verify/
[SLSA build provenance]: https://slsa.dev/spec/v1.0/provenance

## Reporting a vulnerability
Please **do not** open a public issue. Report privately via GitHub Security Advisories:
<https://github.com/jensholdgaard/ourios/security/advisories/new>

You can expect an initial response within 7 days.
