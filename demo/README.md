# open-geocode demo (map QA UI)

A tiny static web UI that drives the open-geocode Runtime API — type to search
addresses (forward + autocomplete), click the map to reverse-geocode — rendered
on a MapLibre GL vector basemap. `open-geocode serve` hosts this UI and the API
from one process, so there's nothing to build and no API key to obtain.

## Run it

Build a Pack (from the repo root):

```powershell
.\scripts\build-pack.ps1
```

Serve the Runtime API and this demo together:

```powershell
cargo run -p open-geocode -- serve --pack .\data\build\pack --demo .\demo --bind 127.0.0.1:8080
```

Open <http://localhost:8080>.

The browser calls `/search` and `/autocomplete` from the command box and
`/reverse` on map click. No address bundle or Pack is loaded into the browser.

## Basemap

The basemap is a keyless, hosted MapLibre vector style — **OpenFreeMap**
(`config.js → styleUrl`). No account, no API key, no self-hosted tiles; it's fine
for low-volume local testing. Point `styleUrl` at any other hosted MapLibre style
if you prefer.

The vendored libraries in `vendor/` (MapLibre GL, PMTiles, Protomaps basemap
themes) are committed so a fresh clone runs with no build or `npm` step. The
glyph/sprite assets under `vendor/fonts/` and `vendor/sprites/` are **not**
committed (~11 MB of font ranges) and aren't needed for the keyless OpenFreeMap
basemap this demo uses.

## Live demo

A hosted version of this demo runs at <https://ajha.ca/open-geocode>.
