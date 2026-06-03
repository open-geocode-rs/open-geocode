# open-geocode Map QA Demo

This demo searches a finished Pack through the Rust Runtime API and displays
returned results on top of OpenStreetMap raster tiles using Leaflet.

Generate the Pack from the repo root:

```powershell
.\scripts\build-pack.ps1
```

Serve the Runtime API and static demo from one process:

```powershell
cargo run -p open-geocode -- serve --pack .\data\build\pack --demo .\demo --bind 127.0.0.1:8080
```

Then open:

```text
http://localhost:8080
```

The browser calls `/autocomplete` as you type, `/search` on submit, and
`/reverse` when the map is clicked. Leaflet and the OpenStreetMap tiles load from
a CDN (pinned with SRI hashes); nothing else is bundled — no Pack or address data
is loaded into the browser.
