# Security Policy

## Supported versions
ourios is pre-1.0 and pre-release. Only the `main` branch receives fixes.

## Verifying releases
Releases are signed by two mechanisms: the container image carries a keyless
cosign signature, and release assets carry SLSA build-provenance attestations.
Verify before you run anything.

**Container image** — signed keyless with [cosign] (Sigstore; the signature is
recorded in the Rekor transparency log, there is no long-lived key). The image
tag drops the leading `v` (a `v0.1.0` tag publishes `:0.1.0`). The identity is
pinned to the exact release tag being verified — substitute both occurrences of
the version to verify a different release:

```sh
cosign verify \
  --certificate-identity 'https://github.com/jensholdgaard/ourios/.github/workflows/image.yml@refs/tags/v0.1.0' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/jensholdgaard/ourios:0.1.0
```

**Release artifacts** — release assets carry [SLSA build provenance]
attestations (see the version scope below). Download an asset from the release
and verify it with the GitHub CLI (the `--repo` bounds the accepted signer
identity to this repo's release workflow):

```sh
gh release download v0.1.0 --pattern 'ourios-server-x86_64-unknown-linux-gnu.tar.xz'
gh attestation verify ourios-server-x86_64-unknown-linux-gnu.tar.xz \
  --repo jensholdgaard/ourios
```

The same `gh attestation verify` works on the other release assets. Binary
archives are attested from `v0.1.0` on; the non-binary assets (SBOMs,
installer, source tarball, checksums) are attested from the release after
`v0.1.0` on. Offline, `cosign verify-blob-attestation` against a downloaded
bundle works too.

[cosign]: https://docs.sigstore.dev/cosign/verifying/verify/
[SLSA build provenance]: https://slsa.dev/spec/v1.0/provenance

## Reporting a vulnerability
Please **do not** open a public issue. Report privately via GitHub Security Advisories:
<https://github.com/jensholdgaard/ourios/security/advisories/new>

You can expect an initial response within 7 days.
