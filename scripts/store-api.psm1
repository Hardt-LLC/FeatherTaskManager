#requires -Version 7.0
# Microsoft Store submission API for MSI/EXE apps:
# https://learn.microsoft.com/windows/apps/publish/store-submission-api
$ErrorActionPreference = 'Stop'

$script:ApiBase = 'https://api.store.microsoft.com'
$script:Scope = 'https://api.store.microsoft.com/.default'
$script:GuidPattern = '^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$'
# Inno Setup's silent switches. The API allows at most 40 characters, and
# /SP- is not needed: Inno Setup 6 disables the startup prompt by default.
$script:DefaultInstallerParameters = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART'
$script:Token = $null
$script:TokenExpires = [datetimeoffset]::MinValue
# Tests replace these to run the pipeline against scripted responses.
$script:Transport = { param($Request) Invoke-StoreHttp $Request }
$script:Sleep = { param([int]$Seconds) Start-Sleep -Seconds $Seconds }

function Set-StoreTestHooks {
    param([scriptblock]$Transport, [string]$Token, [scriptblock]$Sleep)
    if ($Transport) { $script:Transport = $Transport }
    if ($Sleep) { $script:Sleep = $Sleep }
    if ($Token) { $script:Token = $Token; $script:TokenExpires = [datetimeoffset]::UtcNow.AddHours(1) }
}

function Get-StoreMetadataPath([string]$Path) {
    if ($Path) { return $Path }
    if ($env:FEATHER_STORE_METADATA) { return $env:FEATHER_STORE_METADATA }
    return Join-Path (Split-Path -Parent $PSScriptRoot) 'installer\store-metadata.local.json'
}

function Read-StoreConfig([string]$Path) {
    $Path = Get-StoreMetadataPath $Path
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Store metadata was not found at $Path. Run scripts/set-store-credential.ps1 first." }
    $config = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    foreach ($name in @('ProductId', 'TenantId', 'ClientId')) {
        if ([string]$config.$name -notmatch $script:GuidPattern) { throw "Store metadata $name must be a GUID." }
    }
    if ([string]$config.SellerId -notmatch '^\d{1,20}$') { throw 'Store metadata SellerId must be the numeric Partner Center seller ID.' }
    if ($config.CredentialMode -cnotin @('Secret', 'Certificate', 'Environment')) { throw 'Store metadata CredentialMode must be Secret, Certificate or Environment.' }
    if ($config.CredentialMode -eq 'Certificate' -and [string]$config.CertificateThumbprint -notmatch '^[0-9A-Fa-f]{40}$') { throw 'Store metadata CertificateThumbprint must be 40 hex characters in Certificate mode.' }
    if ($config.Architecture -cnotin @('X64', 'X86', 'Arm64', 'Arm', 'Neutral')) { throw 'Store metadata Architecture is invalid.' }
    if (@($config.Languages).Count -lt 1) { throw 'Store metadata must list at least one listing language.' }
    if ([string]$config.PackageRepository -cnotmatch '^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$') { throw 'Store metadata PackageRepository must be owner/name.' }
    if (-not $config.PSObject.Properties['InstallerParameters'] -or [string]::IsNullOrWhiteSpace($config.InstallerParameters)) {
        $config | Add-Member -NotePropertyName InstallerParameters -NotePropertyValue $script:DefaultInstallerParameters -Force
    }
    if (([string]$config.InstallerParameters).Length -gt 40) { throw 'Store metadata InstallerParameters must be at most 40 characters (the Store API limit).' }
    return $config
}

function Get-StoreSecretPath([string]$ClientId) {
    return Join-Path $env:LOCALAPPDATA "FeatherStorePublishing\$ClientId.secret"
}

function ConvertTo-Base64Url([byte[]]$Bytes) {
    return [Convert]::ToBase64String($Bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

function New-StoreClientAssertion {
    param(
        [Parameter(Mandatory)][System.Security.Cryptography.X509Certificates.X509Certificate2]$Certificate,
        [Parameter(Mandatory)][string]$TenantId,
        [Parameter(Mandatory)][string]$ClientId
    )
    $rsa = [System.Security.Cryptography.X509Certificates.RSACertificateExtensions]::GetRSAPrivateKey($Certificate)
    if ($null -eq $rsa) { throw 'The Store API certificate has no usable RSA private key.' }
    $now = [datetimeoffset]::UtcNow.ToUnixTimeSeconds()
    $header = [ordered]@{
        alg = 'RS256'
        typ = 'JWT'
        x5t = ConvertTo-Base64Url $Certificate.GetCertHash()
        'x5t#S256' = ConvertTo-Base64Url ([System.Security.Cryptography.SHA256]::HashData($Certificate.RawData))
    }
    $claims = [ordered]@{
        aud = "https://login.microsoftonline.com/$TenantId/oauth2/v2.0/token"
        iss = $ClientId
        sub = $ClientId
        jti = [guid]::NewGuid().ToString()
        nbf = $now - 60
        iat = $now
        exp = $now + 600
    }
    $utf8 = [Text.Encoding]::UTF8
    $unsigned = (ConvertTo-Base64Url $utf8.GetBytes(($header | ConvertTo-Json -Compress))) + '.' + (ConvertTo-Base64Url $utf8.GetBytes(($claims | ConvertTo-Json -Compress)))
    $signature = $rsa.SignData([Text.Encoding]::ASCII.GetBytes($unsigned), [System.Security.Cryptography.HashAlgorithmName]::SHA256, [System.Security.Cryptography.RSASignaturePadding]::Pkcs1)
    return $unsigned + '.' + (ConvertTo-Base64Url $signature)
}

function Get-StoreAccessToken($Config) {
    if ($script:Token -and [datetimeoffset]::UtcNow -lt $script:TokenExpires.AddMinutes(-5)) { return $script:Token }
    $form = @{ grant_type = 'client_credentials'; client_id = $Config.ClientId; scope = $script:Scope }
    switch ($Config.CredentialMode) {
        'Certificate' {
            $certificate = Get-Item -LiteralPath "Cert:\CurrentUser\My\$($Config.CertificateThumbprint)" -ErrorAction SilentlyContinue
            if ($null -eq $certificate -or -not $certificate.HasPrivateKey) { throw 'The Store API certificate is not in CurrentUser\My with its private key.' }
            if ($certificate.NotAfter -lt (Get-Date)) { throw "The Store API certificate expired on $($certificate.NotAfter). Run scripts/set-store-credential.ps1 -Mode Certificate to rotate it." }
            if ($certificate.NotAfter -lt (Get-Date).AddDays(30)) { Write-Warning "The Store API certificate expires on $($certificate.NotAfter)." }
            $form.client_assertion_type = 'urn:ietf:params:oauth:client-assertion-type:jwt-bearer'
            $form.client_assertion = New-StoreClientAssertion -Certificate $certificate -TenantId $Config.TenantId -ClientId $Config.ClientId
        }
        'Secret' {
            $path = Get-StoreSecretPath $Config.ClientId
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw 'No stored Store API client secret. Run scripts/set-store-credential.ps1.' }
            # DPAPI (current user) protects the stored value; it is never printed.
            $secure = ConvertTo-SecureString -String (Get-Content -LiteralPath $path -Raw).Trim()
            $form.client_secret = [Net.NetworkCredential]::new('', $secure).Password
        }
        'Environment' {
            if ([string]::IsNullOrWhiteSpace($env:FEATHER_STORE_CLIENT_SECRET)) { throw 'Environment mode needs the FEATHER_STORE_CLIENT_SECRET process environment variable.' }
            $form.client_secret = $env:FEATHER_STORE_CLIENT_SECRET
        }
    }
    try {
        $response = Invoke-RestMethod -Method Post -Uri "https://login.microsoftonline.com/$($Config.TenantId)/oauth2/v2.0/token" -Body $form -ContentType 'application/x-www-form-urlencoded' -TimeoutSec 60
    } catch {
        # The error body from Entra names the problem (AADSTS code) without echoing the credential.
        $detail = [string]$_.ErrorDetails.Message
        try { $detail = ($detail | ConvertFrom-Json).error_description.Split("`n")[0].Trim() } catch { }
        throw "Microsoft Entra did not issue a Store API token: $detail"
    } finally {
        $form.Clear()
    }
    $script:Token = $response.access_token
    $script:TokenExpires = [datetimeoffset]::UtcNow.AddSeconds([int]$response.expires_in)
    return $script:Token
}

function Invoke-StoreHttp($Request) {
    $parameters = @{
        Method = $Request.Method
        Uri = $Request.Uri
        Headers = $Request.Headers
        SkipHttpErrorCheck = $true
        MaximumRedirection = 0
        TimeoutSec = 300
    }
    if ($null -ne $Request.Body) {
        $parameters.Body = [Text.Encoding]::UTF8.GetBytes($Request.Body)
        $parameters.ContentType = 'application/json; charset=utf-8'
    }
    if ($null -ne $Request.BodyBytes) {
        $parameters.Body = $Request.BodyBytes
        $parameters.ContentType = $Request.ContentType
    }
    $response = Invoke-WebRequest @parameters
    $header = { param($Name) $value = $response.Headers[$Name]; if ($value) { [string]@($value)[0] } }
    return [pscustomobject]@{
        StatusCode = [int]$response.StatusCode
        Content = [string]$response.Content
        RetryAfter = & $header 'Retry-After'
        CorrelationId = & $header 'X-Correlation-ID'
    }
}

function Invoke-StoreApi {
    param(
        [Parameter(Mandatory)]$Config,
        [Parameter(Mandatory)][ValidateSet('GET', 'POST', 'PUT', 'PATCH')][string]$Method,
        [Parameter(Mandatory)][string]$Path,
        $Body,
        # Returns the whole envelope (isSuccess, errors, responseData) instead of throwing on isSuccess=false.
        [switch]$Envelope
    )
    for ($attempt = 1; ; $attempt++) {
        $request = @{
            Method = $Method
            Uri = $script:ApiBase + $Path
            Headers = @{ Authorization = "Bearer $(Get-StoreAccessToken $Config)"; 'X-Seller-Account-Id' = [string]$Config.SellerId }
            Body = if ($null -ne $Body) { $Body | ConvertTo-Json -Depth 20 -Compress } else { $null }
        }
        $response = & $script:Transport $request
        if ($response.StatusCode -in @(429, 500, 502, 503, 504) -and $attempt -lt 6) {
            $wait = 0
            if (-not [int]::TryParse([string]$response.RetryAfter, [ref]$wait) -or $wait -lt 1) { $wait = [math]::Min(120, 5 * [math]::Pow(2, $attempt)) }
            & $script:Sleep ([math]::Min($wait, 300))
            continue
        }
        $data = $null
        if ($response.Content) { try { $data = $response.Content | ConvertFrom-Json } catch { $data = $null } }
        $messages = if ($data -and $data.PSObject.Properties['errors']) { @($data.errors | Where-Object { $_ } | ForEach-Object { "$($_.target): $($_.message) [$($_.code)]" }) } else { @() }
        $failed = $response.StatusCode -ge 300 -or $null -eq $data -or -not $data.isSuccess
        if ($failed -and -not ($Envelope -and $data -and $response.StatusCode -lt 300)) {
            $detail = if ($messages.Count) { $messages -join '; ' } elseif ($response.Content) { $response.Content.Substring(0, [math]::Min(500, $response.Content.Length)) } else { 'no response body' }
            throw "Store API $Method $Path failed with HTTP $($response.StatusCode): $detail (correlation $($response.CorrelationId))"
        }
        if ($Envelope) { return $data }
        foreach ($message in $messages) { Write-Warning "Store API: $message" }
        return $data.responseData
    }
}

function Get-StoreProductPath($Config, [string]$Suffix) {
    return "/submission/v1/product/$($Config.ProductId)$Suffix"
}

function Get-StoreSubmissionState($Config) {
    $status = Invoke-StoreApi $Config GET (Get-StoreProductPath $Config '/status') -Envelope
    $ongoing = [string]$status.responseData.ongoingSubmissionId
    $publishing = $null
    if ($ongoing) {
        $publishing = Invoke-StoreApi $Config GET (Get-StoreProductPath $Config "/submission/$ongoing/status")
    }
    return [pscustomobject]@{
        IsReady = [bool]$status.responseData.isReady
        OngoingSubmissionId = $ongoing
        PublishingStatus = if ($publishing) { [string]$publishing.publishingStatus } else { $null }
        HasFailed = if ($publishing) { [bool]$publishing.hasFailed } else { $false }
        Errors = @($status.errors | Where-Object { $_ })
    }
}

function Wait-StoreDraftReady {
    param($Config, [int]$TimeoutMinutes = 45, [int]$IntervalSeconds = 15)
    $deadline = [datetimeoffset]::UtcNow.AddMinutes($TimeoutMinutes)
    while ($true) {
        $status = Invoke-StoreApi $Config GET (Get-StoreProductPath $Config '/status') -Envelope
        $errors = @($status.errors | Where-Object { $_ })
        $uploadError = @($errors | Where-Object { $_.code -eq 'packageuploaderror' })
        if ($uploadError.Count) { throw "The Store could not download the package: $($uploadError[0].message)" }
        if ($status.isSuccess -and $status.responseData.isReady) { return }
        if ([datetimeoffset]::UtcNow -gt $deadline) {
            $detail = ($errors | ForEach-Object { "$($_.target): $($_.message)" }) -join '; '
            throw "The Store draft did not become ready within $TimeoutMinutes minutes. $detail"
        }
        & $script:Sleep $IntervalSeconds
    }
}

function Test-StoreListing([hashtable]$Listing, [string]$Language) {
    $limits = @{ description = 10000; whatsNew = 1500; shortDescription = 1000; additionalLicenseTerms = 10000; copyright = 200; developedBy = 255; contactInfo = 200 }
    foreach ($field in $limits.Keys) {
        if ($Listing.ContainsKey($field) -and ([string]$Listing[$field]).Length -gt $limits[$field]) { throw "$Language $field is longer than $($limits[$field]) characters." }
    }
    if ($Listing.ContainsKey('productFeatures')) {
        $features = @($Listing.productFeatures)
        if ($features.Count -gt 20) { throw "$Language has more than 20 product features." }
        foreach ($feature in $features) { if (([string]$feature).Length -gt 200) { throw "$Language has a product feature longer than 200 characters." } }
    }
    if ($Listing.ContainsKey('searchTerms')) {
        $terms = @($Listing.searchTerms)
        if ($terms.Count -gt 7) { throw "$Language has more than 7 search terms." }
        foreach ($term in $terms) { if (([string]$term).Length -gt 30) { throw "$Language search term '$term' is longer than 30 characters." } }
        $words = @($terms | ForEach-Object { ([string]$_).ToLowerInvariant() -split '\s+' } | Where-Object { $_ } | Sort-Object -Unique)
        if ($words.Count -gt 21) { throw "$Language search terms use more than 21 unique words." }
    }
    if ($Listing.ContainsKey('requirements')) {
        $items = @($Listing.requirements | ForEach-Object { $_.minimumHardware; $_.recommendedHardware } | Where-Object { $_ })
        if ($items.Count -gt 11) { throw "$Language has more than 11 hardware requirements." }
        foreach ($item in $items) { if (([string]$item).Length -gt 200) { throw "$Language has a hardware requirement longer than 200 characters." } }
    }
}

function Read-StoreListingFiles {
    param([Parameter(Mandatory)][string]$Directory, [Parameter(Mandatory)][string[]]$Languages)
    $root = Split-Path -Parent $PSScriptRoot
    $result = @{}
    foreach ($language in $Languages) {
        $path = Join-Path $Directory "$language.json"
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing Store listing file $path." }
        $listing = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json -AsHashtable
        if ($listing.ContainsKey('additionalLicenseTermsFile')) {
            $listing.additionalLicenseTerms = (Get-Content -LiteralPath (Join-Path $root $listing.additionalLicenseTermsFile) -Raw).Trim()
            $listing.Remove('additionalLicenseTermsFile')
        }
        if ($listing.ContainsKey('language') -and $listing.language -cne $language) { throw "$path declares language $($listing.language)." }
        $listing.language = $language
        Test-StoreListing $listing $language
        $result[$language] = $listing
    }
    return $result
}

function Read-StoreWhatsNew {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string[]]$Languages)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Missing Store release notes $Path (one 'What's new' text per listing language)." }
    $notes = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json -AsHashtable
    $result = @{}
    foreach ($language in $Languages) {
        $text = [string]$notes[$language]
        if ([string]::IsNullOrWhiteSpace($text)) { throw "$Path has no What's new text for $language." }
        $text = $text.Replace("`r`n", "`n").Trim()
        Test-StoreListing @{ whatsNew = $text } $language
        $result[$language] = $text
    }
    foreach ($key in $notes.Keys) { if ($key -cnotin $Languages) { throw "$Path has text for $key, which is not a configured listing language." } }
    return $result
}

function Get-StoreAssetPlan([string]$Directory) {
    $files = @(Get-ChildItem -LiteralPath $Directory -Filter '*.png' -File | Sort-Object Name)
    $logos = @($files | Where-Object { $_.Name -match '^(boxart|poster)-' })
    $screenshots = @($files | Where-Object { $_.Name -notmatch '^(boxart|poster|logo)-' })
    if ($screenshots.Count -lt 1 -or $screenshots.Count -gt 10) { throw 'A Store listing needs 1 to 10 screenshots.' }
    if (@($logos | Where-Object { $_.Name -match '^boxart-' }).Count -ne 1 -or $logos.Count -gt 2) { throw 'Store logos must be one boxart-*.png (1:1) and at most one poster-*.png (2:3).' }
    return [pscustomobject]@{ Screenshots = $screenshots; Logos = $logos }
}

function Publish-StoreListingAssets {
    param($Config, [string]$Language, $Plan)
    $created = Invoke-StoreApi $Config POST (Get-StoreProductPath $Config '/listings/assets/create') @{
        language = $Language
        createAssetRequest = @{ Screenshot = $Plan.Screenshots.Count; Logo = $Plan.Logos.Count }
    }
    $slots = $created.listingAssets
    $commit = @{ language = $Language; screenshots = @(); storeLogos = @() }
    foreach ($kind in @('screenshots', 'storeLogos')) {
        $files = @(if ($kind -eq 'screenshots') { $Plan.Screenshots } else { $Plan.Logos })
        $targets = @($slots.$kind)
        if ($targets.Count -ne $files.Count) { throw "The Store returned $($targets.Count) $kind upload slots for $($files.Count) files." }
        for ($i = 0; $i -lt $files.Count; $i++) {
            $headers = @{ 'x-ms-blob-type' = 'BlockBlob' }
            if ($targets[$i].PSObject.Properties['httpHeaders'] -and $targets[$i].httpHeaders) {
                foreach ($property in $targets[$i].httpHeaders.PSObject.Properties) { $headers[$property.Name] = [string]$property.Value }
            }
            $upload = & $script:Transport @{
                Method = if ($targets[$i].httpMethod) { [string]$targets[$i].httpMethod } else { 'PUT' }
                Uri = [string]$targets[$i].primaryAssetUploadUrl
                Headers = $headers
                BodyBytes = [IO.File]::ReadAllBytes($files[$i].FullName)
                ContentType = 'image/png'
            }
            if ($upload.StatusCode -lt 200 -or $upload.StatusCode -ge 300) { throw "Uploading $($files[$i].Name) failed with HTTP $($upload.StatusCode)." }
            $commit[$kind] += @{ id = [string]$targets[$i].id; assetUrl = [string]$targets[$i].primaryAssetUploadUrl }
        }
    }
    $null = Invoke-StoreApi $Config PUT (Get-StoreProductPath $Config '/listings/assets/commit') @{ listingAssets = $commit }
}

function Update-StoreDraft {
    param(
        [Parameter(Mandatory)]$Config,
        [Parameter(Mandatory)][string]$PackageUrl,
        [Parameter(Mandatory)][hashtable]$WhatsNew,
        [hashtable]$Listings,
        [string]$AssetsDirectory,
        [switch]$Submit,
        [int]$ReadyTimeoutMinutes = 45
    )
    $uri = [uri]$PackageUrl
    if ($uri.Scheme -ne 'https' -or $uri.Query -or -not $uri.AbsolutePath.EndsWith('.exe')) { throw 'The package URL must be a direct HTTPS link to an .exe without a query string.' }
    $assetPlan = if ($AssetsDirectory) { Get-StoreAssetPlan $AssetsDirectory } else { $null }
    $state = Get-StoreSubmissionState $Config
    if ($state.OngoingSubmissionId) {
        throw "Submission $($state.OngoingSubmissionId) is $($state.PublishingStatus). The draft can be changed only after it finishes; check with scripts/store-status.ps1."
    }
    $changes = [Collections.Generic.List[string]]::new()

    $packages = @((Invoke-StoreApi $Config GET (Get-StoreProductPath $Config '/packages')).packages)
    $target = @($packages | Where-Object { $_.packageType -eq 'exe' -and @($_.architectures) -contains $Config.Architecture })
    if ($target.Count -ne 1) { throw "Expected exactly one $($Config.Architecture) EXE package in the draft; found $($target.Count)." }
    if ([string]$target[0].packageUrl -cne $PackageUrl -or [string]$target[0].installerParameters -cne $Config.InstallerParameters) {
        $patch = [ordered]@{}
        foreach ($property in $target[0].PSObject.Properties) { if ($property.Name -ne 'packageId') { $patch[$property.Name] = $property.Value } }
        $patch.packageUrl = $PackageUrl
        $patch.installerParameters = [string]$Config.InstallerParameters
        $null = Invoke-StoreApi $Config PATCH (Get-StoreProductPath $Config "/packages/$($target[0].packageId)") $patch
        $null = Invoke-StoreApi $Config POST (Get-StoreProductPath $Config '/packages/commit')
        Wait-StoreDraftReady $Config -TimeoutMinutes $ReadyTimeoutMinutes
        $changes.Add("package $($Config.Architecture) -> $PackageUrl")
    }

    $languages = @($Config.Languages)
    $current = @((Invoke-StoreApi $Config GET (Get-StoreProductPath $Config "/metadata/listings?languages=$($languages -join ',')")).listings)
    foreach ($language in $languages) {
        $existing = @($current | Where-Object { $_.language -eq $language })
        if ($existing.Count -ne 1) { throw "The draft has no $language listing. Add the language in Partner Center first." }
        $listing = if ($Listings) { $Listings[$language].Clone() } else { @{ language = $language } }
        $listing.whatsNew = $WhatsNew[$language]
        $oldWhatsNew = ([string]$existing[0].whatsNew).Replace("`r`n", "`n").Trim()
        if ($Listings -or $oldWhatsNew -cne $listing.whatsNew) {
            $null = Invoke-StoreApi $Config PATCH (Get-StoreProductPath $Config '/metadata') @{ listings = $listing }
            $changes.Add("$language listing")
        }
        if ($assetPlan) {
            Publish-StoreListingAssets $Config $language $assetPlan
            $changes.Add("$language screenshots and logos")
        }
    }

    # Any package upload has already finished; a draft that stays unready now has validation errors.
    Wait-StoreDraftReady $Config -TimeoutMinutes 5
    $submissionId = $null
    if ($Submit) {
        $submitted = Invoke-StoreApi $Config POST (Get-StoreProductPath $Config '/submit')
        $submissionId = [string]$submitted.submissionId
        if (-not $submissionId) { throw 'The Store accepted the submission request but returned no submission ID.' }
    }
    return [pscustomobject]@{ Changes = @($changes); Submitted = [bool]$Submit; SubmissionId = $submissionId }
}

function Add-StorePackageReadmeRow {
    param([Parameter(Mandatory)][string]$Readme, [Parameter(Mandatory)][string]$Version, [Parameter(Mandatory)][string]$FileName)
    $row = "| $Version | [$FileName]($Version/$FileName) | see [SHA256SUMS.txt]($Version/SHA256SUMS.txt) |"
    if ($Readme.Contains("| $Version |")) { return $Readme }
    $lines = [Collections.Generic.List[string]]::new([string[]]($Readme -split "`r?`n"))
    $header = $lines.IndexOf('| Version | Installer | SHA-256 |')
    if ($header -lt 0 -or $header + 1 -ge $lines.Count -or $lines[$header + 1] -notmatch '^\|\s*-') { throw 'The package repository README has no installer table.' }
    # Newest version first.
    $lines.Insert($header + 2, $row)
    return ($lines -join "`n")
}

Export-ModuleMember -Function Read-StoreConfig, Get-StoreMetadataPath, Get-StoreSecretPath, New-StoreClientAssertion, Invoke-StoreApi, Get-StoreSubmissionState, Wait-StoreDraftReady, Test-StoreListing, Read-StoreListingFiles, Read-StoreWhatsNew, Get-StoreAssetPlan, Update-StoreDraft, Add-StorePackageReadmeRow, Set-StoreTestHooks, Get-StoreProductPath
