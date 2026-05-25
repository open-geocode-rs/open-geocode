param(
    [string]$InputPath = ".\data\ontario.pbf",
    [string]$OutputPath = ".\data\build\normalized-records.ndjson",
    [string]$RejectedOutputPath = ".\data\build\rejected-records.ndjson",
    [string]$ReportPath = ".\data\build\build-report.json"
)

$ErrorActionPreference = "Stop"

cargo run -p open-geocode -- normalize-osm `
    --input $InputPath `
    --output $OutputPath `
    --rejected-output $RejectedOutputPath `
    --report $ReportPath

if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
