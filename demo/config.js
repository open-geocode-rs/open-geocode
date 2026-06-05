// Public, non-secret runtime config for the demo UI. Loaded before app.js; sets
// window.OPEN_GEOCODE_CONFIG. This is the BARE-CLONE config used by
// `open-geocode serve --demo` — zero setup, no API key, no self-hosted tiles:
//
//   - styleUrl — a hosted, keyless MapLibre vector style (OpenFreeMap "bright").
//     The browser fetches the style, its tiles, glyphs, and sprites from
//     OpenFreeMap; nothing here is self-hosted and no account is needed. Fine for
//     low-volume local testing. Swap it for any hosted MapLibre style URL (e.g.
//     .../styles/positron or .../styles/liberty) if you prefer.
//
//     To self-host the tiles instead, omit styleUrl and set `pmtilesUrl` to a
//     Protomaps PMTiles basemap; app.js then builds that style from vendored
//     glyphs/sprites. Same MapLibre renderer either way — only this file changes.
//
//   - apiBase — empty: `open-geocode serve` hosts this UI and the API (/search,
//     /autocomplete, /reverse) on the SAME origin at the root.
window.OPEN_GEOCODE_CONFIG = {
  apiBase: "",
  styleUrl: "https://tiles.openfreemap.org/styles/bright",
};
