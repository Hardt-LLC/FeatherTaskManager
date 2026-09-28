#requires -Version 7.0
# Offline checks for the Microsoft Store pipeline against a scripted Store API.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Import-Module (Join-Path $PSScriptRoot 'store-api.psm1') -Force
$fixture = Join-Path $root ('target\security-review\store-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $fixture
$checks = 0
function Assert([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "FAIL: $Message" }
    $script:checks++
}
function Expect-Throws([scriptblock]$Action, [string]$Reason) {
    try { & $Action | Out-Null }
    catch {
        if ($_.Exception.Message.Contains($Reason)) { $script:checks++; return }
        throw "FAIL: expected '$Reason' but got: $($_.Exception.Message)"
    }
    throw "FAIL: expected a failure containing '$Reason'"
}
function From-Base64Url([string]$Value) {
    $value = $Value.Replace('-', '+').Replace('_', '/')
    switch ($value.Length % 4) { 2 { $value += '==' } 3 { $value += '=' } }
    return [Convert]::FromBase64String($value)
}

$tenant = [guid]::NewGuid().ToString()
$client = [guid]::NewGuid().ToString()
$configPath = Join-Path $fixture 'store-metadata.json'
$metadata = [ordered]@{
    ProductId = 'e42e60ce-30cf-4d70-807d-f04095ce11c2'; SellerId = '12345678'; TenantId = $tenant; ClientId = $client
    CredentialMode = 'Environment'; CertificateThumbprint = ''; PackageRepository = 'Hardt-LLC/feather-store-packages'
    Architecture = 'X64'; Languages = @('en-us', 'ko-kr')
}
$metadata | ConvertTo-Json | Set-Content -LiteralPath $configPath
$config = Read-StoreConfig $configPath
foreach ($bad in @(@{ SellerId = 'seller' }, @{ CredentialMode = 'Browser' }, @{ ClientId = 'not-a-guid' }, @{ CredentialMode = 'Certificate' })) {
    $copy = [ordered]@{} + $metadata
    foreach ($key in $bad.Keys) { $copy[$key] = $bad[$key] }
    $badPath = Join-Path $fixture 'bad.json'
    $copy | ConvertTo-Json | Set-Content -LiteralPath $badPath
    Expect-Throws { Read-StoreConfig $badPath } 'Store metadata'
}

$oldUrl = 'https://raw.githubusercontent.com/Hardt-LLC/feather-store-packages/92140552d8f76f98a6487e6fbd08df69d8bc2d23/2026.9.4/FeatherTaskManager-2026.9.4-Setup-x64.exe'
$newUrl = 'https://raw.githubusercontent.com/Hardt-LLC/feather-store-packages/0123456789abcdef0123456789abcdef01234567/2026.9.5/FeatherTaskManager-2026.9.5-Setup-x64.exe'
# One shared object: the hooks close over it, so they still see it while
# publish-store.ps1 (a different script scope) is running.
$server = @{}
function New-Server {
    param([string]$Ongoing = '', [switch]$UploadError, [int]$PackageCount = 1, [int]$Throttle = 0, [switch]$RejectListing)
    $server.Clear()
    $server.Calls = [Collections.Generic.List[object]]::new(); $server.Ongoing = $Ongoing; $server.NotReady = 0; $server.UploadError = [bool]$UploadError
    $server.PackageCount = $PackageCount; $server.Throttle = $Throttle; $server.RejectListing = [bool]$RejectListing
    $server.PackageUrl = $oldUrl; $server.WhatsNew = @{ 'en-us' = 'old'; 'ko-kr' = 'old' }; $server.Sleeps = [Collections.Generic.List[int]]::new()
}
$transport = {
    param($Request)
    $s = $server
    $uri = [uri]$Request.Uri
    $body = if ($Request.Body) { $Request.Body | ConvertFrom-Json } else { $null }
    $s.Calls.Add([pscustomobject]@{ Method = $Request.Method; Path = $uri.PathAndQuery; Host = $uri.Host; Body = $body; Headers = $Request.Headers; Bytes = $Request.BodyBytes })
    $reply = { param($Data, [int]$Code = 200) [pscustomobject]@{ StatusCode = $Code; Content = ($Data | ConvertTo-Json -Depth 20); RetryAfter = $null; CorrelationId = 'fixture' } }
    if ($uri.Host -eq 'upload.example') { return & $reply @{} 201 }
    if ($s.Throttle -gt 0) { $s.Throttle--; return [pscustomobject]@{ StatusCode = 429; Content = ''; RetryAfter = '7'; CorrelationId = 'fixture' } }
    $product = '/submission/v1/product/e42e60ce-30cf-4d70-807d-f04095ce11c2'
    $path = $uri.AbsolutePath
    switch -Regex ("$($Request.Method) $path") {
        '^GET .*/status$' {
            if ($path -match '/submission/(\d+)/status$') { return & $reply @{ isSuccess = $true; responseData = @{ publishingStatus = 'INPROGRESS'; hasFailed = $false } } }
            if ($s.UploadError) { return & $reply @{ isSuccess = $true; errors = @(@{ code = 'packageuploaderror'; message = 'HTTP 404'; target = 'packages' }); responseData = @{ isReady = $false; ongoingSubmissionId = '' } } }
            $ready = $s.NotReady -le 0
            if (-not $ready) { $s.NotReady-- }
            return & $reply @{ isSuccess = $true; responseData = @{ isReady = $ready -and -not $s.Ongoing; ongoingSubmissionId = $s.Ongoing } }
        }
        '^GET .*/packages$' {
            $packages = @(1..$s.PackageCount | ForEach-Object {
                @{ packageId = "p$_"; packageUrl = $s.PackageUrl; languages = @('en-us', 'ko-kr'); architectures = @('X64'); isSilentInstall = $false
                   installerParameters = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-'; genericDocUrl = 'https://jrsoftware.org/ishelp/topic_setupexitcodes.htm'
                   errorDetails = @(@{ errorScenario = 'rebootRequired'; errorScenarioDetails = @(@{ errorValue = '8'; errorUrl = '' }) }); packageType = 'exe' }
            })
            return & $reply @{ isSuccess = $true; responseData = @{ packages = $packages } }
        }
        '^PATCH .*/packages/p1$' { $s.PackageUrl = $body.packageUrl; return & $reply @{ isSuccess = $true; responseData = @{ pollingUrl = ''; ongoingSubmissionId = '' } } }
        '^POST .*/packages/commit$' { $s.NotReady = 1; return & $reply @{ isSuccess = $true; responseData = @{ pollingUrl = "$product/status"; ongoingSubmissionId = '' } } }
        '^GET .*/metadata/listings$' {
            $listings = @('en-us', 'ko-kr' | ForEach-Object { @{ language = $_; whatsNew = $s.WhatsNew[$_]; description = 'fixture' } })
            return & $reply @{ isSuccess = $true; responseData = @{ listings = $listings } }
        }
        '^PATCH .*/metadata$' {
            if ($s.RejectListing) { return & $reply @{ isSuccess = $false; errors = @(@{ code = 'badrequest'; message = 'whatsNew is invalid'; target = 'listings' }) } 400 }
            $s.WhatsNew[$body.listings.language] = $body.listings.whatsNew
            return & $reply @{ isSuccess = $true; responseData = @{ pollingUrl = ''; ongoingSubmissionId = '' } }
        }
        '^POST .*/listings/assets/create$' {
            $slot = { param($Kind, $Index) @{ id = "$Kind$Index"; primaryAssetUploadUrl = "https://upload.example/$Kind$Index`?sig=x"; secondaryAssetUploadUrl = ''; httpMethod = 'PUT'; httpHeaders = @{} } }
            $shots = @(1..$body.createAssetRequest.Screenshot | ForEach-Object { & $slot 'shot' $_ })
            $logos = @(1..$body.createAssetRequest.Logo | ForEach-Object { & $slot 'logo' $_ })
            return & $reply @{ isSuccess = $true; responseData = @{ listingAssets = @{ language = $body.language; screenshots = $shots; storeLogos = $logos } } }
        }
        '^PUT .*/listings/assets/commit$' { return & $reply @{ isSuccess = $true; responseData = @{} } }
        '^POST .*/submit$' { return & $reply @{ isSuccess = $true; responseData = @{ submissionId = '1152921505699'; pollingUrl = ''; ongoingSubmissionId = '' } } }
    }
    throw "Unexpected fixture request $($Request.Method) $($uri.PathAndQuery)"
}
Set-StoreTestHooks -Transport $transport.GetNewClosure() -Token 'fixture-token' -Sleep { param([int]$Seconds) $server.Sleeps.Add($Seconds) }.GetNewClosure()
$notes = @{ 'en-us' = 'Version 2026.9.5'; 'ko-kr' = '2026.9.5 버전' }
function Get-Writes { @($server.Calls | Where-Object { $_.Method -ne 'GET' -and $_.Host -eq 'api.store.microsoft.com' }) }

# A release updates the package URL, commits it, waits for the upload and sets What's new per language.
New-Server
$result = Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes
$writes = Get-Writes
$patch = @($writes | Where-Object { $_.Path -like '*/packages/p1' })
Assert ($patch.Count -eq 1 -and $patch[0].Body.packageUrl -ceq $newUrl) 'the package URL is patched once'
Assert ($patch[0].Body.installerParameters -ceq '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-' -and $patch[0].Body.errorDetails[0].errorScenario -eq 'rebootRequired') 'package fields other than the URL are preserved'
Assert (-not $patch[0].Body.PSObject.Properties['packageId']) 'the package ID is not sent in the body'
Assert (@($writes | Where-Object { $_.Path -like '*/packages/commit' }).Count -eq 1) 'packages are committed'
$listingWrites = @($writes | Where-Object { $_.Path -like '*/metadata' })
Assert ($listingWrites.Count -eq 2 -and ($listingWrites.Body.listings.language -join ',') -eq 'en-us,ko-kr') 'both listings are patched'
Assert ($listingWrites[1].Body.listings.whatsNew -ceq '2026.9.5 버전' -and -not $listingWrites[1].Body.listings.PSObject.Properties['description']) 'only What''s new is sent without -SyncListings'
Assert (-not @($writes | Where-Object { $_.Path -like '*/submit' }).Count -and -not $result.Submitted) 'nothing is submitted without -Submit'
Assert (@($server.Calls | Where-Object { $_.Headers.Authorization -cne 'Bearer fixture-token' -or $_.Headers['X-Seller-Account-Id'] -cne '12345678' }).Count -eq 0) 'every call carries the token and seller ID'
Assert ($result.Changes.Count -eq 3) 'the result lists the package and two listings'
Assert ($server.Sleeps.Count -ge 1) 'the upload wait polled the draft status'

# Running again changes nothing, and -Submit then only submits.
$server.Calls.Clear()
$again = Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes -Submit
$writes = Get-Writes
Assert ($writes.Count -eq 1 -and $writes[0].Path -like '*/submit') 'a repeated run only submits'
Assert ($again.Submitted -and $again.SubmissionId -ceq '1152921505699' -and $again.Changes.Count -eq 0) 'the submission ID is returned'

# -SyncListings sends the full repository listing, including the license text.
$listings = Read-StoreListingFiles -Directory (Join-Path $root 'store\listings') -Languages @('en-us', 'ko-kr')
$server.Calls.Clear()
$null = Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes -Listings $listings
$full = @(Get-Writes | Where-Object { $_.Path -like '*/metadata' })
Assert ($full.Count -eq 2 -and $full[0].Body.listings.additionalLicenseTerms.StartsWith('MIT License') -and @($full[0].Body.listings.productFeatures).Count -eq 20) 'the full listing is sent with the license terms'
Assert (-not $full[0].Body.listings.PSObject.Properties['additionalLicenseTermsFile']) 'the license file reference is resolved locally'

# Screenshots and logos are uploaded to the returned slots and committed in order.
$assets = Join-Path $fixture 'assets'
$null = New-Item -ItemType Directory -Path $assets
foreach ($file in @('02-performance.png', '01-processes.png', 'boxart-2160x2160.png', 'poster-1440x2160.png', 'logo-300x300.png')) { [IO.File]::WriteAllBytes((Join-Path $assets $file), [byte[]](137, 80, 78, 71)) }
$server.Calls.Clear()
$null = Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes -AssetsDirectory $assets
$uploads = @($server.Calls | Where-Object { $_.Host -eq 'upload.example' })
$commits = @($server.Calls | Where-Object { $_.Path -like '*/listings/assets/commit' })
Assert ($uploads.Count -eq 8 -and $uploads[0].Headers['x-ms-blob-type'] -ceq 'BlockBlob' -and $uploads[0].Bytes.Length -eq 4) 'two screenshots and two logos are uploaded per language'
Assert ($commits.Count -eq 2 -and ($commits[0].Body.listingAssets.screenshots.id -join ',') -eq 'shot1,shot2' -and @($commits[0].Body.listingAssets.storeLogos).Count -eq 2) 'uploaded assets are committed'
$badAssets = Join-Path $fixture 'bad-assets'
$null = New-Item -ItemType Directory -Path $badAssets
[IO.File]::WriteAllBytes((Join-Path $badAssets '01.png'), [byte[]](1))
Expect-Throws { Get-StoreAssetPlan $badAssets } 'boxart'

# Failures stop before anything unsafe happens.
New-Server -Ongoing '1152921505000'
Expect-Throws { Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes } 'INPROGRESS'
Assert ((Get-Writes).Count -eq 0) 'nothing is written while a submission is in certification'
New-Server -PackageCount 2
Expect-Throws { Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes } 'exactly one X64'
New-Server -UploadError
Expect-Throws { Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes } 'could not download'
New-Server -RejectListing
Expect-Throws { Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes } 'whatsNew is invalid'
New-Server
Expect-Throws { Update-StoreDraft -Config $config -PackageUrl 'http://example.com/setup.exe' -WhatsNew $notes } 'direct HTTPS'
Expect-Throws { Update-StoreDraft -Config $config -PackageUrl "$newUrl`?x=1" -WhatsNew $notes } 'direct HTTPS'
New-Server -Throttle 2
$null = Update-StoreDraft -Config $config -PackageUrl $newUrl -WhatsNew $notes
Assert (($server.Sleeps | Select-Object -First 2) -join ',' -eq '7,7') 'Retry-After is honoured on HTTP 429'

# The entry script wires the same flow; a given package URL skips staging.
New-Server
$notesPath = Join-Path $fixture '2026.9.5.store.json'
$notes | ConvertTo-Json | Set-Content -LiteralPath $notesPath
$publish = Join-Path $PSScriptRoot 'publish-store.ps1'
$published = & $publish -Version '2026.9.5' -PackageUrl $newUrl -WhatsNewFile $notesPath -MetadataPath $configPath -Submit 6>$null
Assert ($published.Submitted -and $published.SubmissionId -ceq '1152921505699' -and $server.PackageUrl -ceq $newUrl) 'publish-store.ps1 updates the draft and submits'
Assert ($server.WhatsNew['ko-kr'] -ceq '2026.9.5 버전') 'publish-store.ps1 reads What''s new as UTF-8'
Expect-Throws { & $publish -Version '2026.9.6' -PackageUrl $newUrl -WhatsNewFile $notesPath -MetadataPath $configPath 6>$null } 'not the 2026.9.6 installer'
New-Server -Ongoing '1152921505000'
Expect-Throws { & $publish -Version '2026.9.5' -WhatsNewFile $notesPath -MetadataPath $configPath 6>$null } 'Wait until it finishes'
Assert ((Get-Writes).Count -eq 0 -and -not (Test-Path -LiteralPath (Join-Path $root 'target\store-staging\2026.9.5'))) 'an ongoing submission stops publishing before staging'

# Local listing limits.
$whatsNewPath = Join-Path $fixture 'notes.json'
@{ 'en-us' = 'x' * 1501; 'ko-kr' = 'ok' } | ConvertTo-Json | Set-Content -LiteralPath $whatsNewPath
Expect-Throws { Read-StoreWhatsNew -Path $whatsNewPath -Languages @('en-us', 'ko-kr') } 'longer than 1500'
@{ 'en-us' = 'ok' } | ConvertTo-Json | Set-Content -LiteralPath $whatsNewPath
Expect-Throws { Read-StoreWhatsNew -Path $whatsNewPath -Languages @('en-us', 'ko-kr') } 'no What''s new text for ko-kr'
@{ 'en-us' = 'ok'; 'ko-kr' = 'ok'; 'ja-jp' = 'ok' } | ConvertTo-Json | Set-Content -LiteralPath $whatsNewPath
Expect-Throws { Read-StoreWhatsNew -Path $whatsNewPath -Languages @('en-us', 'ko-kr') } 'not a configured listing language'
Expect-Throws { Test-StoreListing @{ searchTerms = @(1..8 | ForEach-Object { "term$_" }) } 'en-us' } 'more than 7 search terms'
Expect-Throws { Test-StoreListing @{ searchTerms = @('a b c d e f g', 'h i j k l m n', 'o p q r s t u v') } 'en-us' } 'more than 21 unique words'
Expect-Throws { Test-StoreListing @{ searchTerms = @('x' * 31) } 'en-us' } 'longer than 30'
Expect-Throws { Test-StoreListing @{ productFeatures = @('x' * 201) } 'en-us' } 'longer than 200'
$null = Read-StoreWhatsNew -Path (Join-Path $root 'releases\2026.9.4.store.json') -Languages @('en-us', 'ko-kr')
$checks++

# Certificate credential: a signed RS256 client assertion bound to the certificate.
$rsa = [Security.Cryptography.RSA]::Create(2048)
$request = [Security.Cryptography.X509Certificates.CertificateRequest]::new('CN=Store fixture', $rsa, [Security.Cryptography.HashAlgorithmName]::SHA256, [Security.Cryptography.RSASignaturePadding]::Pkcs1)
$certificate = $request.CreateSelfSigned([datetimeoffset]::UtcNow.AddMinutes(-5), [datetimeoffset]::UtcNow.AddDays(1))
$jwt = New-StoreClientAssertion -Certificate $certificate -TenantId $tenant -ClientId $client
$parts = $jwt.Split('.')
$header = [Text.Encoding]::UTF8.GetString((From-Base64Url $parts[0])) | ConvertFrom-Json
$claims = [Text.Encoding]::UTF8.GetString((From-Base64Url $parts[1])) | ConvertFrom-Json
$expectedThumb = [Convert]::ToBase64String([Security.Cryptography.SHA256]::HashData($certificate.RawData)).TrimEnd('=').Replace('+', '-').Replace('/', '_')
Assert ($parts.Count -eq 3 -and $header.alg -ceq 'RS256' -and $header.'x5t#S256' -ceq $expectedThumb) 'the assertion header names the certificate'
Assert ($claims.aud -ceq "https://login.microsoftonline.com/$tenant/oauth2/v2.0/token" -and $claims.iss -ceq $client -and $claims.sub -ceq $client -and ($claims.exp - $claims.iat) -eq 600) 'the assertion claims target the tenant token endpoint'
$publicKey = [Security.Cryptography.X509Certificates.RSACertificateExtensions]::GetRSAPublicKey($certificate)
$verified = $publicKey.VerifyData([Text.Encoding]::ASCII.GetBytes("$($parts[0]).$($parts[1])"), (From-Base64Url $parts[2]), [Security.Cryptography.HashAlgorithmName]::SHA256, [Security.Cryptography.RSASignaturePadding]::Pkcs1)
Assert $verified 'the assertion signature verifies with the certificate public key'

# Package repository README keeps the newest version first and never duplicates a row.
$readme = "# Packages`n`n| Version | Installer | SHA-256 |`n| --- | --- | --- |`n| 2026.9.4 | old | old |`n"
$updated = Add-StorePackageReadmeRow -Readme $readme -Version '2026.9.5' -FileName 'FeatherTaskManager-2026.9.5-Setup-x64.exe'
Assert (($updated -split "`n")[4].StartsWith('| 2026.9.5 | [FeatherTaskManager-2026.9.5-Setup-x64.exe](2026.9.5/') -and ($updated -split "`n")[5].StartsWith('| 2026.9.4 |')) 'the new row goes above older versions'
Assert ((Add-StorePackageReadmeRow -Readme $updated -Version '2026.9.5' -FileName 'x.exe') -ceq $updated) 'an existing row is kept as is'
Expect-Throws { Add-StorePackageReadmeRow -Readme '# none' -Version '2026.9.5' -FileName 'x.exe' } 'no installer table'

Remove-Item -LiteralPath $fixture -Recurse -Force
Write-Output "PASS: Store pipeline passed $checks checks."
