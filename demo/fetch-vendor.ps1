# Fetches the demo's third-party frontend assets into demo/vendor/.
# These are regenerable artifacts (like node_modules) and are gitignored.
# Run from anywhere:  ./demo/fetch-vendor.ps1
$ErrorActionPreference = "Stop"
$vendor = Join-Path $PSScriptRoot "vendor"
New-Item -ItemType Directory -Force $vendor | Out-Null

# Pinned library versions.
Invoke-WebRequest -UseBasicParsing "https://unpkg.com/maplibre-gl@5.24.0/dist/maplibre-gl.js"  -OutFile (Join-Path $vendor "maplibre-gl.js")
Invoke-WebRequest -UseBasicParsing "https://unpkg.com/maplibre-gl@5.24.0/dist/maplibre-gl.css" -OutFile (Join-Path $vendor "maplibre-gl.css")
Invoke-WebRequest -UseBasicParsing "https://unpkg.com/pmtiles@3.2.0/dist/pmtiles.js"           -OutFile (Join-Path $vendor "pmtiles.js")
# Self-contained (deps inlined) build of the Protomaps style layers.
Invoke-WebRequest -UseBasicParsing "https://esm.sh/@protomaps/basemaps@5.7.2/es2022/basemaps.bundle.mjs" -OutFile (Join-Path $vendor "basemaps.js")

# Fonts (glyphs) + sprites from the Protomaps basemaps-assets repo.
$zip = Join-Path $env:TEMP "basemaps-assets.zip"
$tmp = Join-Path $env:TEMP "basemaps-assets"
Invoke-WebRequest -UseBasicParsing "https://github.com/protomaps/basemaps-assets/archive/refs/heads/main.zip" -OutFile $zip
Expand-Archive -Path $zip -DestinationPath $tmp -Force
$root = Join-Path $tmp "basemaps-assets-main"
Copy-Item -Recurse -Force (Join-Path $root "fonts")   (Join-Path $vendor "fonts")
Copy-Item -Recurse -Force (Join-Path $root "sprites") (Join-Path $vendor "sprites")
Remove-Item -Recurse -Force $zip, $tmp

Write-Host "Vendored frontend assets into $vendor"
