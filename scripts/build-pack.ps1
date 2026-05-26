param(
    [string]$InputPath = ".\data\ontario.pbf",
    [string]$PackPath = ".\data\build\pack"
)

$ErrorActionPreference = "Stop"

cargo run -p open-geocode -- build `
    --input $InputPath `
    --pack $PackPath

if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
