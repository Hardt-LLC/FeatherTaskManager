# Signed Windows releases

Releases use Azure **Artifact Signing** with the `Hardt-Cert` Public Trust profile. The expected certificate subject is `CN=HARDT, O=HARDT, L=Casper, S=Wyoming, C=US`. The non-secret account metadata is in `installer/signing-metadata.example.json`; authentication secrets never belong in the repository or release ZIP.

The release pipeline first checks the app's PE loader policy (System32-only static imports, ASLR, DEP and high-entropy VA), then signs the executable **before** embedding it in Setup. Inno Setup invokes the same signer for its uninstaller and final installer. Every signature must validate under Windows Authenticode, match the expected publisher, and have a timestamp. Failed checks stop the release. There is no unsigned release fallback.

## Local release

Use PowerShell 7, Rust 1.98.0/MSVC, .NET 8, and Inno Setup 6.7.3. The installed compiler is accepted only when its Authenticode publisher and pinned SHA-256 match. For a workspace-local compiler and the pinned Microsoft PowerShell module:

```powershell
./scripts/prepare-release-tools.ps1
```

This downloads the official Inno Setup release, checks its published SHA-256 and publisher, and uses its `/PORTABLE=1` mode under `target/tools`. The Microsoft `ArtifactSigning` module is saved there at version `0.1.8`, the version used by the official Azure action `v2.0.0`. Existing local installations can be used with `prepare-release-tools.ps1 -SkipCompiler` and `FEATHER_INNO_COMPILER`.

Provide `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, and `AZURE_CLIENT_SECRET` as process environment variables using your credential manager. They identify a service principal with signing permission for the certificate profile. Scripts do not print these variables or persist them. Set the non-secret signing inputs:

```powershell
$env:FEATHER_SIGNING_METADATA = (Resolve-Path ./installer/signing-metadata.example.json).Path
$env:FEATHER_SIGNING_SUBJECT = 'CN=HARDT, O=HARDT, L=Casper, S=Wyoming, C=US'
./scripts/build-release.ps1
```

`build-release.ps1` produces versioned portable EXE, installer EXE, portable ZIP, `SHA256SUMS.txt`, and `release-manifest.json`. The manifest records the exact hashes, sizes, publisher, version, and build time. Signing changes the executable hash, so hashes and the ZIP are generated only after signing.

Create and push the matching tag after review, then publish with the prepared release notes:

```powershell
./scripts/publish-release.ps1 -NotesFile ./releases/2026.9.3.md
```

Publishing requires clean tracked source at the exact local release tag, with that commit in `origin/main` history. Immediately before creating the release it resolves the target GitHub repository's tag, including annotated tags, and checks the remote commit matches the local source. It also checks hashes, signatures and the app loader policy, and refuses to overwrite a release. Only the three explicitly named versioned assets and their manifests are uploaded. Fetch current remote refs before publishing; tags should be protected against modification.

After the GitHub release, update the Microsoft Store with `./scripts/publish-store.ps1`; see [STORE.md](STORE.md).

## GitHub Actions

Dispatch `Signed Windows release` from `main` against an existing version tag. Trusted inline workflow code checks the exact tag commit and main-branch ancestry **before** executing tagged scripts. The build job has read-only repository permission, checkout does not persist credentials, and Azure credentials are supplied only to the signing step after compilation. Publishing runs on a separate fresh runner with repository write permission and no Azure credentials; it downloads only the signed artifact from the same workflow run and checks the source again.

Before enabling hosted signing, configure the `release` environment with a **main-only deployment branch rule** and appropriate reviewers, then add `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, and `AZURE_CLIENT_SECRET` there. YAML alone cannot stop a repository writer from creating a different workflow that requests secrets. These repository settings are an administrator's prerequisite, not something the local release scripts configure. `publish=false` saves verified artifacts; `publish=true` also publishes. Local releases work without storing any credential in GitHub.

For a future secretless workflow, configure a narrowly scoped GitHub OIDC federated identity for the `release` environment, grant it certificate-profile signing permission, and use the official `azure/login` action with `id-token: write`. After login, set `FEATHER_SIGNING_CREDENTIAL_MODE=AzureCli` and replace the signing step's secret environment. The signer supports this explicit Azure CLI mode; it never silently switches identity. See the [official Azure authentication examples](https://github.com/Azure/artifact-signing-action/blob/v2.0.0/README.md). The workflow is dispatch-only, so pushing a tag does not start an unconfigured signing run.

The account endpoint and certificate profile must already exist and pass identity validation. These scripts do not provision paid Azure resources. A valid Authenticode signature identifies the publisher; it does not guarantee that Windows SmartScreen will suppress every reputation prompt.

## Version convention

Versions use `yyyy.m.x`: `2026.9.1` is the first release in September 2026; the next is `2026.9.2`. The counter starts at `1` in a new month. Windows PE resources use a fourth zero component, such as `2026.9.1.0`.

`scripts/bump-version.ps1 -Preview` shows the next version. Run it without `-Preview` to update Cargo.toml, the local package entry in Cargo.lock, app.rc, and the application manifest together. An explicit later `-Version` is also accepted. `scripts/version.ps1` rejects inconsistent metadata before packaging; release notes are maintained separately.

Official references: [Artifact Signing integration](https://learn.microsoft.com/en-us/azure/artifact-signing/how-to-signing-integrations), [Azure action v2.0.0](https://github.com/Azure/artifact-signing-action/tree/v2.0.0), [signed Inno uninstallers](https://jrsoftware.org/ishelp/topic_setup_signeduninstaller.htm), [Inno signing tools](https://jrsoftware.org/ishelp/topic_setup_signtool.htm), [Inno release 6.7.3](https://github.com/jrsoftware/issrc/releases/tag/is-6_7_3).
