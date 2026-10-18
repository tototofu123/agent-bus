<#
.SYNOPSIS
    Verify opencode can actually reach the NVIDIA models agent-bus depends on.

.DESCRIPTION
    Checks the five things that have each broken independently, and reports
    each as PASS or FAIL with the reason. Read-only: it edits nothing.

    JSONC is hand-commented and the deprecated `provider` key is load-bearing
    here, so this script does not rewrite the config. Rewriting it with a
    JSON parser silently discards the comments, which document why the
    deprecated key is still in use.

    See docs/models.md for the reasoning behind each check.

.EXAMPLE
    pwsh -File tools/setup-opencode-models.ps1
#>
[CmdletBinding()]
param(
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'

$configPath = Join-Path $env:USERPROFILE '.config\opencode\opencode.jsonc'
$authPath   = Join-Path $env:USERPROFILE '.local\share\opencode\auth.json'

$expectedModels = @(
    'stepfun-ai/step-5-preview'
    'nvidia/nemotron-3-super-120b-a12b'
    'nvidia/nemotron-3-nano-30b-a3b'
    'z-ai/glm-5.3-flash'
    'deepseek-ai/deepseek-v4.1-flash'
)

$results = [System.Collections.Generic.List[object]]::new()

function Add-Result {
    param([string]$Name, [bool]$Pass, [string]$Detail)
    $results.Add([pscustomobject]@{ Check = $Name; Pass = $Pass; Detail = $Detail })
}

function Test-Step {
    param([string]$Name, [scriptblock]$Body)
    try {
        $r = & $Body
        Add-Result $Name $r.Pass $r.Detail
    } catch {
        Add-Result $Name $false $_.Exception.Message
    }
}

# --- 1. config file present -------------------------------------------------
Test-Step 'config exists' {
    if (-not (Test-Path $configPath)) {
        return [pscustomobject]@{ Pass = $false; Detail = "not found at $configPath" }
    }
    return [pscustomobject]@{ Pass = $true; Detail = $configPath }
}

$config = if (Test-Path $configPath) { [System.IO.File]::ReadAllText($configPath) } else { '' }

# --- 2. nvidia-live provider block -----------------------------------------
Test-Step 'nvidia-live provider declared' {
    if ($config -notmatch '"nvidia-live"\s*:\s*\{') {
        return [pscustomobject]@{ Pass = $false; Detail = 'no nvidia-live provider block in opencode.jsonc' }
    }
    return [pscustomobject]@{ Pass = $true; Detail = 'block present' }
}

# --- 3. every expected model named in that block ---------------------------
Test-Step 'all models listed' {
    $missing = $expectedModels | Where-Object { $config -notmatch [regex]::Escape('"' + $_ + '"') }
    if ($missing) {
        return [pscustomobject]@{ Pass = $false; Detail = ("not listed: " + ($missing -join ', ')) }
    }
    return [pscustomobject]@{ Pass = $true; Detail = "$($expectedModels.Count) models named" }
}

# --- 4. no apiKey option on nvidia-live ------------------------------------
# An explicit apiKey beats the credential store. {env:...} reads the
# environment at startup, so inside a running opencode it is empty and the
# request goes out unauthenticated.
Test-Step 'no shadowing apiKey option' {
    $blockMatch = [regex]::Match($config, '"nvidia-live"\s*:\s*\{(?<body>.*?)\n\s*\}', 'Singleline')
    if (-not $blockMatch.Success) {
        return [pscustomobject]@{ Pass = $false; Detail = 'could not isolate the nvidia-live block' }
    }
    if ($blockMatch.Groups['body'].Value -match '"apiKey"') {
        return [pscustomobject]@{ Pass = $false; Detail = 'nvidia-live sets apiKey; it overrides the credential store' }
    }
    return [pscustomobject]@{ Pass = $true; Detail = 'key resolves from auth.json' }
}

# --- 5. policy allow --------------------------------------------------------
Test-Step 'policy allows nvidia-live' {
    if ($config -notmatch '"effect"\s*:\s*"allow"\s*,\s*\r?\n\s*"resource"\s*:\s*"nvidia-live"') {
        return [pscustomobject]@{ Pass = $false; Detail = 'denied by the default-deny provider.use policy' }
    }
    return [pscustomobject]@{ Pass = $true; Detail = 'allowlisted' }
}

# --- 6. credential present --------------------------------------------------
Test-Step 'credential in auth.json' {
    if (-not (Test-Path $authPath)) {
        return [pscustomobject]@{ Pass = $false; Detail = "not found at $authPath" }
    }
    $auth = [System.IO.File]::ReadAllText($authPath) | ConvertFrom-Json
    $entry = $auth.PSObject.Properties['nvidia-live']
    if (-not $entry) {
        return [pscustomobject]@{ Pass = $false; Detail = 'no nvidia-live entry; opencode will need a restart after adding one' }
    }
    if (-not $entry.Value.key) {
        return [pscustomobject]@{ Pass = $false; Detail = 'nvidia-live entry has no key' }
    }
    return [pscustomobject]@{ Pass = $true; Detail = 'key present' }
}

# --- 7. credential still works upstream -------------------------------------
# The only check that catches a rotated or revoked key. Costs one request.
Test-Step 'key accepted by NVIDIA' {
    if (-not (Test-Path $authPath)) { throw 'no auth.json' }
    $auth = [System.IO.File]::ReadAllText($authPath) | ConvertFrom-Json
    $key = $auth.PSObject.Properties['nvidia-live'].Value.key
    if (-not $key) { throw 'no nvidia-live key' }
    try {
        $null = Invoke-RestMethod -Uri 'https://integrate.api.nvidia.com/v1/models' `
            -Headers @{ Authorization = "Bearer $key" } -TimeoutSec 20
    } catch {
        $code = try { [int]$_.Exception.Response.StatusCode } catch { '---' }
        throw "HTTP $code"
    }
    return [pscustomobject]@{ Pass = $true; Detail = 'endpoint reachable' }
}

# --- report -----------------------------------------------------------------
if (-not $Quiet) {
    $width = ($results | ForEach-Object { $_.Check.Length } | Measure-Object -Maximum).Maximum
    foreach ($r in $results) {
        $mark = if ($r.Pass) { 'PASS' } else { 'FAIL' }
        '{0}  {1}  {2}' -f $mark, $r.Check.PadRight($width), $r.Detail
    }
    ''
}

$failed = @($results | Where-Object { -not $_.Pass })
if ($failed.Count) {
    "  $($failed.Count) of $($results.Count) checks failed."
    exit 1
}
"  all $($results.Count) checks passed."
exit 0