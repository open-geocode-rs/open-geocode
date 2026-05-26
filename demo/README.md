# open-geocode Map QA Demo

This demo overlays a small Toronto subset from a binary Pack on top of
OpenStreetMap raster tiles using Leaflet.

Generate the Pack and demo data from the repo root:

```powershell
.\scripts\build-pack.ps1
.\scripts\extract-toronto-demo.ps1
```

By default, the generated subset contains 2,500 point records and 500 centroid
records. Override the sample size with `-PointLimit` and `-CentroidLimit`.

Serve the static files:

```powershell
python -m http.server 5173 --directory .\demo
```

Then open:

```text
http://localhost:5173
```

The generated `demo/data/toronto-addresses.js` file is ignored by git.
