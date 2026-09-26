#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Repository,
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)][string]$ExpectedCommit
)
$ErrorActionPreference = 'Stop'
if ($Repository -cnotmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -or
    $Tag -cnotmatch '^v20\d{2}\.([1-9]|1[0-2])\.([1-9]\d{0,4})$' -or
    $ExpectedCommit -cnotmatch '^[0-9a-f]{40}$') {
    throw 'Invalid remote release identity.'
}
function Read-GitHubObject([string]$Route) {
    $response = & gh api $Route
    if ($LASTEXITCODE -ne 0) { throw 'The remote release tag could not be verified.' }
    try { return $response | ConvertFrom-Json -ErrorAction Stop }
    catch { throw 'Invalid GitHub tag response.' }
}
$reference = Read-GitHubObject "repos/$Repository/git/ref/tags/$Tag"
if ($reference.ref -cne "refs/tags/$Tag") { throw 'GitHub returned an unexpected tag reference.' }
$object = $reference.object
for ($depth = 0; $depth -lt 8; $depth++) {
    if ($object.sha -cnotmatch '^[0-9a-f]{40}$') { throw 'Invalid remote tag object.' }
    if ($object.type -ceq 'commit') {
        if ($object.sha -cne $ExpectedCommit) { throw 'Remote release tag does not match the verified local source commit.' }
        return
    }
    if ($object.type -cne 'tag') { throw 'The remote tag does not resolve to a commit.' }
    $annotated = Read-GitHubObject "repos/$Repository/git/tags/$($object.sha)"
    $object = $annotated.object
}
throw 'Remote tag nesting exceeds the supported limit.'
