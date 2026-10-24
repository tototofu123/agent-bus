# Verify opencode can reach the NVIDIA models agent-bus depends on.
#
# Read-only: edits nothing. The config is JSONC and hand-commented, and the
# deprecated `provider` key is load-bearing here, so this script does not
# rewrite it.
#
# See docs/models.md for the reasoning behind each check.

[CmdletBinding()]
param([switch]$Quiet)

$ErrorActionPreference = 'Stop'

$configPath = Join-Path $env:USERPROFILE '.config\opencode\opencode.jsonc'
$cli = Join-Path $env:APPDATA 'npm\node_modules\@opencode\cli\bin\opencode.exe'

# Models opencode serves through its built-in `nvidia` provider.
$inCatalog = @(
    'stepfun-ai/step-5-preview'
    'z-ai/glm-5.3-flash'
    'deepseek-ai/deepseek-v4.1-flash'
)

# Ids that appear in catalogs but cannot be called on this account. Assigning one
# to an omp role fails at call time -- or worse, silently falls through to
# openrouter and dies on a credits wall.
$knownDead = @(
    'nvidia/nemotron-3-super-120b-a12b'
    'nvidia/llama-3.1-nemotron-ultra-253b-v1'
)

$ompConfig = Join-Path $env:USERPROFILE '.omp\agent\config.yml'

$results = [System.Collections.Generic.List[object]]::new()
function Add-Result($n, $p, $d, $warn) {
    $results.Add([pscustomobject]@{Check=$n;Pass=$p;Warn=[bool]$warn;Detail=$d})
}
function Test-Step($n, [scriptblock]$b) {
    try { $r = & $b; Add-Result $n $r.Pass $r.Detail $r.Warn }
    catch { Add-Result $n $false $_.Exception.Message }
}

# 1. config present, and not the 50-byte stub opencode left behind on 2026-10-21
Test-Step 'config is intact' {
    if (-not (Test-Path $configPath)) { return @{Pass=$false;Detail="not found at $configPath"} }
    $t = [System.IO.File]::ReadAllText($configPath)
    if ($t.Length -lt 1000) { return @{Pass=$false;Detail="only $($t.Length) bytes; looks wiped"} }
    if ($t -notmatch '"provider"\s*:\s*\{') { return @{Pass=$false;Detail='no provider block'} }
    @{Pass=$true;Detail="$([math]::Round($t.Length/1024,1)) KB"}
}

# 2. no hand-maintained whitelist to disagree with the catalog
Test-Step 'no stale whitelist' {
    $t = [System.IO.File]::ReadAllText($configPath)
    if ($t -notmatch '"whitelist"\s*:\s*\[') { @{Pass=$true;Detail='full catalogs exposed'} }
    else { @{Pass=$false;Detail='whitelist present; it disagrees with the catalog silently'} }
}

# 3. every in-catalog model is visible to opencode
Test-Step 'models visible to opencode' {
    $out = (& $cli models 2>&1) | Out-String
    if (-not $out.Trim()) { throw 'opencode models returned nothing' }
    $missing = $inCatalog | Where-Object { $out -notmatch [regex]::Escape("nvidia/$_") }
    if ($missing) { @{Pass=$false;Detail=("not listed: " + ($missing -join ', '))} }
    else { @{Pass=$true;Detail="$($inCatalog.Count) of $($inCatalog.Count)"} }
}

# 4. credential present for nvidia
Test-Step 'nvidia credential stored' {
    $list = (& $cli auth list 2>&1) | Out-String
    # (?m) matters: Out-String yields one blob, so ^ would only match position 0.
    if ($list -match '(?m)^\s*Nvidia\s') { @{Pass=$true;Detail='stored'} }
    else { @{Pass=$false;Detail='no nvidia credential in auth list'} }
}

# 5. the credential actually authenticates
$live = $null
Test-Step 'key accepted by NVIDIA' {
    $exp = (& $cli auth export 2>&1) | Out-String | ConvertFrom-Json
    $key = ($exp | Where-Object { $_.integrationID -eq 'nvidia' }).value.key
    if (-not $key) { throw 'no nvidia key in auth export' }
    try { $script:live = Invoke-RestMethod -Uri 'https://integrate.api.nvidia.com/v1/models' `
        -Headers @{ Authorization = "Bearer $key" } -TimeoutSec 20
    } catch {
        $code = try { [int]$_.Exception.Response.StatusCode } catch { '---' }
        throw "HTTP $code"
    }
    @{Pass=$true;Detail="$($script:live.data.Count) models live"}
}

# 6. no omp role points at a model that only looks callable
Test-Step 'omp roles avoid dead ids' {
    if (-not (Test-Path $ompConfig)) { return @{Pass=$true;Detail='no omp config'} }
    $bad = @()
    foreach ($line in [System.IO.File]::ReadAllLines($ompConfig)) {
        if ($line -notmatch '^\s+\w+:\s*(\S+)') { continue }
        $id = $Matches[1] -replace ':.*$', ''
        if ($knownDead -contains $id) { $bad += $line.Trim() }
    }
    if ($bad) { @{Pass=$false;Detail=("dead id in role: " + ($bad -join '; '))} }
    else { @{Pass=$true;Detail="$($knownDead.Count) dead ids absent"} }
}

# 7. one model actually answers
Test-Step 'step-5-preview answers' {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $r = (& $cli run --standalone --model 'nvidia/stepfun-ai/step-5-preview' `
        'Reply with exactly one word: PONG' 2>&1) | Out-String
    $sw.Stop()
    if ($r -match 'PONG') { @{Pass=$true;Detail="$([math]::Round($sw.Elapsed.TotalSeconds,1))s"} }
    else { @{Pass=$false;Detail="no PONG; $((($r -split '`n') | Select-Object -Last 1).Trim())"} }
}

if (-not $Quiet) {
    $w = ($results | ForEach-Object { $_.Check.Length } | Measure-Object -Maximum).Maximum
    foreach ($r in $results) {
        $mark = if (-not $r.Pass) { 'FAIL' } elseif ($r.Warn) { 'WARN' } else { 'PASS' }
        '{0}  {1}  {2}' -f $mark, $r.Check.PadRight($w), $r.Detail
    }
    ''
}
$fail = @($results | Where-Object { -not $_.Pass })
if ($fail.Count) { "  $($fail.Count) of $($results.Count) checks failed."; exit 1 }
"  all $($results.Count) checks passed."
exit 0
