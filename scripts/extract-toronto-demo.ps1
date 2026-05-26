param(
    [string]$PackPath = ".\data\build\pack",
    [string]$OutputPath = ".\demo\data\toronto-addresses.js",
    [double]$MinLon = -79.65,
    [double]$MinLat = 43.55,
    [double]$MaxLon = -79.10,
    [double]$MaxLat = 43.90,
    [int]$PointLimit = 2500,
    [int]$CentroidLimit = 500,
    [int]$InspectLimit = 200000
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $PackPath)) {
    throw "Pack not found: $PackPath"
}

$outputDir = Split-Path -Parent $OutputPath
if ($outputDir -and -not (Test-Path -LiteralPath $outputDir)) {
    New-Item -ItemType Directory -Path $outputDir | Out-Null
}

$records = New-Object "System.Collections.Generic.List[object]"
$points = 0
$centroids = 0

$jsonText = cargo run --quiet -p open-geocode -- inspect-pack `
    --pack $PackPath `
    --layer address `
    --limit $InspectLimit

if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$addressRecords = ($jsonText -join [Environment]::NewLine) | ConvertFrom-Json
$scanned = 0

foreach ($root in $addressRecords) {
    $scanned++

    if ($root.geometry.type -ne "Point") {
        continue
    }

    $lon = [double]$root.geometry.coordinates[0]
    $lat = [double]$root.geometry.coordinates[1]
    $precision = $root.location_precision

    if ($lat -lt $MinLat -or $lat -gt $MaxLat -or $lon -lt $MinLon -or $lon -gt $MaxLon) {
        continue
    }

    if ($precision -eq "point" -and $points -ge $PointLimit) {
        continue
    }
    if ($precision -eq "centroid" -and $centroids -ge $CentroidLimit) {
        continue
    }

    $records.Add([pscustomobject][ordered]@{
        id = $root.id
        label = $root.label
        lat = $lat
        lon = $lon
        precision = $precision
    })

    if ($precision -eq "point") {
        $points++
    }
    elseif ($precision -eq "centroid") {
        $centroids++
    }

    if ($points -ge $PointLimit -and $centroids -ge $CentroidLimit) {
        break
    }
}

$json = $records | ConvertTo-Json -Depth 4 -Compress
Set-Content -LiteralPath $OutputPath -Value "window.OPEN_GEOCODE_TORONTO_RECORDS = $json;" -Encoding UTF8

[pscustomobject]@{
    Scanned = $scanned
    Written = $records.Count
    Points = $points
    Centroids = $centroids
    Output = (Resolve-Path -LiteralPath $OutputPath).Path
}
