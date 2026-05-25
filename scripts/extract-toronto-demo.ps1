param(
    [string]$InputPath = ".\data\build\normalized-records.ndjson",
    [string]$OutputPath = ".\demo\data\toronto-addresses.js",
    [double]$MinLon = -79.65,
    [double]$MinLat = 43.55,
    [double]$MaxLon = -79.10,
    [double]$MaxLat = 43.90,
    [int]$PointLimit = 2500,
    [int]$CentroidLimit = 500
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $InputPath)) {
    throw "Input file not found: $InputPath"
}

$outputDir = Split-Path -Parent $OutputPath
if ($outputDir -and -not (Test-Path -LiteralPath $outputDir)) {
    New-Item -ItemType Directory -Path $outputDir | Out-Null
}

$records = New-Object "System.Collections.Generic.List[object]"
$reader = [System.IO.File]::OpenText((Resolve-Path -LiteralPath $InputPath))
$scanned = 0
$points = 0
$centroids = 0

try {
    while (($line = $reader.ReadLine()) -ne $null) {
        $scanned++

        if ([string]::IsNullOrWhiteSpace($line)) {
            continue
        }

        $root = $line | ConvertFrom-Json
        if ($root.layer -ne "address" -or $root.geometry.type -ne "Point") {
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
}
finally {
    $reader.Dispose()
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
