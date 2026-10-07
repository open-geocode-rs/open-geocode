<#
Score public Nominatim on a POI answer key (see `sample-poi-fixture`).

Every response is cached in -CachePath, so a rerun scores the cached
responses and sends no requests. Uncached cases are fetched under the
Nominatim usage policy: one request per second, an identifying User-Agent.
An expected object that /lookup does not know is counted as absent in
Nominatim (its data differs from the extract) rather than as a miss.
#>
param(
    [string]$FixturePath = ".\fixtures\queries\ontario-poi.json",
    [string]$CachePath = ".\fixtures\queries\ontario-poi-nominatim.json",
    [string]$Endpoint = "https://nominatim.openstreetmap.org",
    [string]$UserAgent = "open-geocode-poi-validation/0.1 (+https://github.com/open-geocode-rs/open-geocode)"
)

$ErrorActionPreference = "Stop"
$RequestIntervalMs = 1100
$LookupBatch = 50
$SaveEvery = 25
# Enough of each result to score and inspect it; licence and bounding boxes
# are dropped to keep the cache small.
$ResultFields = "place_id", "osm_type", "osm_id", "lat", "lon", "category", "type",
    "place_rank", "importance", "addresstype", "name", "display_name"
$Utf8 = New-Object System.Text.UTF8Encoding $false

function Read-Json([string]$Path) {
    [System.IO.File]::ReadAllText((Resolve-Path $Path), $Utf8) | ConvertFrom-Json
}

function Save-Cache {
    $json = $cache | ConvertTo-Json -Depth 8 -Compress
    [System.IO.File]::WriteAllText($CacheFullPath, $json, $Utf8)
}

$script:lastRequest = $null
function Invoke-Nominatim([string]$PathAndQuery) {
    if ($script:lastRequest) {
        $wait = $RequestIntervalMs - $script:lastRequest.ElapsedMilliseconds
        if ($wait -gt 0) { Start-Sleep -Milliseconds $wait }
    }
    $script:lastRequest = [System.Diagnostics.Stopwatch]::StartNew()
    # Windows PowerShell passes a JSON array on as one object; unroll it.
    $response = Invoke-RestMethod -Uri "$Endpoint$PathAndQuery" -UserAgent $UserAgent
    $response
}

function Get-OsmKey([string]$Type, $Id) {
    "$($Type.Substring(0, 1).ToUpper())$Id"
}

$cases = (Read-Json $FixturePath).search
$CacheFullPath = [System.IO.Path]::GetFullPath($CachePath)
if (Test-Path $CacheFullPath) {
    $cache = Read-Json $CacheFullPath
} else {
    $cache = [pscustomobject]@{
        endpoint = $Endpoint
        user_agent = $UserAgent
        retrieved = (Get-Date).ToUniversalTime().ToString("yyyy-MM-dd")
        search = [pscustomobject]@{}
        lookup = [pscustomobject]@{}
    }
}

$fetched = 0
foreach ($case in $cases) {
    if ($cache.search.PSObject.Properties[$case.q]) { continue }
    $query = [uri]::EscapeDataString($case.q)
    $results = @(Invoke-Nominatim "/search?format=jsonv2&q=$query&countrycodes=ca&limit=5")
    $kept = @($results | Select-Object -Property $ResultFields)
    $cache.search | Add-Member -NotePropertyName $case.q -NotePropertyValue $kept
    # Saving rewrites the whole cache, so an interrupted run loses at most a
    # few requests rather than paying for a rewrite after each one.
    $fetched++
    if ($fetched % $SaveEvery -eq 0) { Save-Cache }
}
if ($fetched % $SaveEvery -ne 0) { Save-Cache }

$missing = @($cases | ForEach-Object { Get-OsmKey $_.expect.osm_type $_.expect.osm_id } |
    Where-Object { -not $cache.lookup.PSObject.Properties[$_] } | Select-Object -Unique)
for ($start = 0; $start -lt $missing.Count; $start += $LookupBatch) {
    $batch = $missing[$start..([Math]::Min($start + $LookupBatch, $missing.Count) - 1)]
    $found = @(Invoke-Nominatim "/lookup?format=jsonv2&osm_ids=$($batch -join ',')")
    $known = @($found | ForEach-Object { Get-OsmKey $_.osm_type $_.osm_id })
    foreach ($key in $batch) {
        $cache.lookup | Add-Member -NotePropertyName $key -NotePropertyValue ($known -contains $key)
    }
    Save-Cache
}

$hitAt1 = 0; $hitAt5 = 0; $absent = 0
foreach ($case in $cases) {
    $key = Get-OsmKey $case.expect.osm_type $case.expect.osm_id
    $results = @($cache.search.($case.q))
    $rank = 0
    for ($index = 0; $index -lt $results.Count; $index++) {
        if ((Get-OsmKey $results[$index].osm_type $results[$index].osm_id) -eq $key) {
            $rank = $index + 1
            break
        }
    }
    if ($rank -eq 1) { $hitAt1++ }
    if ($rank -ge 1 -and $rank -le 5) { $hitAt5++ }
    if (-not $cache.lookup.$key) { $absent++ }
}
$present = $cases.Count - $absent
[pscustomobject]@{
    cases = $cases.Count
    hit_at_1 = $hitAt1
    hit_at_5 = $hitAt5
    absent_in_nominatim = $absent
    present_cases = $present
    hit_at_1_rate = [Math]::Round($hitAt1 / $cases.Count, 4)
    hit_at_5_rate = [Math]::Round($hitAt5 / $cases.Count, 4)
    hit_at_1_rate_present = if ($present) { [Math]::Round($hitAt1 / $present, 4) } else { $null }
    hit_at_5_rate_present = if ($present) { [Math]::Round($hitAt5 / $present, 4) } else { $null }
    retrieved = $cache.retrieved
} | ConvertTo-Json
