// Public, non-secret config for the demo UI. Loaded before app.js.
//
// The basemap is OpenStreetMap's free public tiles via Leaflet — keyless, no
// account, fine for local/low-volume use (keep the attribution). Swap `tileUrl`
// for any other {z}/{x}/{y} tile server if you prefer. `apiBase` is empty because
// `open-geocode serve` serves this UI and the API (/search, /autocomplete,
// /reverse) on the same origin.
window.OPEN_GEOCODE_CONFIG = {
  apiBase: "",
  tileUrl: "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
  tileAttribution:
    '&copy; <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a> contributors',
};
