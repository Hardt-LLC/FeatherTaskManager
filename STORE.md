# Microsoft Store releases

Feather is listed in the Microsoft Store as an EXE app. The Store downloads the signed Setup from a direct, versioned HTTPS URL. After the first manual submission, every update goes through the [Microsoft Store submission API for MSI/EXE apps](https://learn.microsoft.com/windows/apps/publish/store-submission-api) from these scripts, without Partner Center in a browser.

| Script | What it does |
| --- | --- |
| `set-store-credential.ps1` | One-time setup and key rotation. Writes the non-secret `installer/store-metadata.local.json` (ignored by Git) and stores the client secret with DPAPI for the current Windows user. |
| `store-status.ps1` | Read-only. Shows draft readiness, any submission in certification, the package URL and each language's What's new. Use it as the connection test. |
| `stage-store-package.ps1` | Copies the Setup asset of the immutable GitHub release into [feather-store-packages](https://github.com/Hardt-LLC/feather-store-packages) and returns a commit-pinned raw URL. |
| `publish-store.ps1` | Stages the package, updates the draft's package URL and What's new, waits until the Store has downloaded the installer, and with `-Submit` sends the draft to certification. |
| `test-store-pipeline.ps1` | Offline checks against a scripted Store API. No credentials or network. |

## One-time setup

The Store API accepts only a Microsoft Entra application that belongs to the tenant associated with the **Partner Center account**. The Azure account used for code signing may be a different account; it plays no part here.

1. In Partner Center, open **Account settings → Organization profile → Tenants**. If no tenant is listed, choose **Create** to make a new Microsoft Entra tenant, or **Associate** one where you can sign in as Global Administrator.
2. Open **Account settings → User management → Microsoft Entra applications**. Choose **Create Microsoft Entra application**, or **Add** an existing app from the associated tenant. Give it the **Manager** role.
3. Open the application. Copy the **Tenant ID** and **Client ID**, then choose **Add new key** and copy the key. Partner Center shows the key only once. Note its expiry date.
4. Copy the **Seller ID** from **Account settings → Legal info → Developer**.
5. In your own terminal (the key is typed hidden, so this cannot run unattended). The scripts need PowerShell 7 (`pwsh`), not Windows PowerShell 5.1. Without an installed `pwsh`, run them through the portable copy, for example `& .\target\tools\pwsh-7.6.6\pwsh.exe -NoProfile -File .\scripts\store-status.ps1`.

   ```powershell
   ./scripts/set-store-credential.ps1 -TenantId <tenant-id> -ClientId <client-id> -SellerId <seller-id>
   ./scripts/store-status.ps1
   ```

The key is stored under `%LOCALAPPDATA%\FeatherStorePublishing`, encrypted with DPAPI so that only the same Windows user on the same PC can read it. The scripts never print it. To rotate the key, add a new key in Partner Center and run `set-store-credential.ps1` again.

**Certificate instead of a key.** If you can manage the app registration in the tenant's Entra admin center, run `set-store-credential.ps1 ... -Mode Certificate`. It creates a non-exportable RSA key in `Cert:\CurrentUser\My` and exports only the public `.cer` under `target/store-credential`. Upload that file under **App registrations → the app → Certificates & secrets → Certificates**. Tokens are then requested with a signed client assertion, and no secret exists anywhere.

**CI.** `CredentialMode: Environment` reads the key from `FEATHER_STORE_CLIENT_SECRET`. Use it only in a protected, main-only environment. It is not configured in this repository.

## Each release

The GitHub release must already be published. It is immutable, so the Store package cannot drift from it.

1. Write `releases/<version>.store.json` with one What's new text (at most 1,500 characters) for each listing language. See [2026.9.4.store.json](releases/2026.9.4.store.json).
2. Set `FEATHER_SIGNING_SUBJECT` to the expected publisher, as for the GitHub release, then update the draft:

   ```powershell
   ./scripts/publish-store.ps1
   ```

   This stages the installer, sets the draft's x64 package URL and silent switches, commits the package, waits until the Store has downloaded it, and sets What's new. Re-running it changes nothing that is already current. The switches come from `InstallerParameters` in the metadata (default `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`). The API accepts at most 40 characters, and `/SP-` is not needed because Inno Setup 6 already disables the startup prompt.
3. Check the draft with `./scripts/store-status.ps1`, then send it to certification:

   ```powershell
   ./scripts/publish-store.ps1 -Submit
   ./scripts/store-status.ps1 -SubmissionId <printed id>
   ```

Certification usually takes up to three business days. Microsoft emails the certification report. Partner Center shows the details.

Options:

- `-SyncListings` also sends the full description, features, search terms, requirements, copyright and license terms from `store/listings/<language>.json`. The license terms come from `LICENSE`. All Store length limits are checked locally first.
- `-AssetsDirectory <folder>` replaces screenshots and logos in every language. Screenshots are the `*.png` files not named `boxart-`, `poster-` or `logo-`, in name order (1–10). Logos are one `boxart-*.png` (1:1, 1080 or 2160 px) and an optional `poster-*.png` (2:3). The published set for each version lives in `store-assets/<version>/` of the package repository. Screenshots must be renders of the design prototype with simulated data, never captures of a real PC.
- `-PackageUrl <url>` skips staging and uses an existing direct URL.

## Safety rules

- The package repository never replaces a file. If `<version>/` already holds different bytes, staging stops; cut a new version instead. URLs pin the commit SHA, so the bytes behind a submitted URL cannot change.
- Before anything is pushed or changed, the release asset must match GitHub's SHA-256 digest, carry a valid timestamped HARDT signature and the right version, and be served byte-for-byte by the raw URL without a redirect.
- While a submission is in certification the Store rejects draft changes. `publish-store.ps1` stops before staging and names the submission.
- Without `-Submit` only the draft changes. The draft can still be reviewed or discarded in Partner Center.
- The first certification (2026.9.4) reported that silent install could not be verified, most likely because of the UAC prompt of the per-machine installer. If certification rejects it, change the installer; the pipeline itself does not need to change.
