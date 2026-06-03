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

The browser calls `/search` from the command input and calls `/reverse` when the
map is clicked. It does not load a generated address bundle or a Pack into the
browser.
