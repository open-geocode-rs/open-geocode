// Shared demo helpers for pages that render the same MapLibre basemap and
// command-style overlay UI.
import { layers, namedFlavor } from "https://esm.sh/@protomaps/basemaps@5.7.2";

// The app's mount, derived from the document <base>. At ajha.ca/open-geocode
// this is "/open-geocode/"; at the root it is "/".
export const BASE = new URL(".", document.baseURI).pathname;

// Public, per-environment config (config.js) selects how demo pages talk to
// interchangeable backends and basemap styles.
export const cfg = window.OPEN_GEOCODE_CONFIG ?? {};

// API base. `open-geocode serve` hosts the UI and the API (/search,
// /autocomplete, /reverse) on the same origin at the root, so config sets
// apiBase:"". When omitted it falls back to <BASE>api.
export const API_BASE = cfg.apiBase ?? `${BASE}api`;

// PMTiles URL for the self-hosted Protomaps basemap; unused when config supplies
// a hosted styleUrl.
const PMTILES_URL = cfg.pmtilesUrl;

export const emptyFeatureCollection = { type: "FeatureCollection", features: [] };

// Every API answer is a GeoJSON FeatureCollection (ADR 0022). These read one
// of its features.
export function featurePoint(feature) {
  const [lon, lat] = feature.geometry.coordinates;
  return { lon, lat };
}

// A result row's two lines: the thing itself, then where it is. The label
// leads with the name where it can; a street or place labelled by its name
// alone is placed by the areas it lies in.
export function featureLines(feature) {
  const { name, label, context = {} } = feature.properties;
  if (label.startsWith(`${name}, `)) {
    return { title: name, subtitle: label.slice(name.length + 2) };
  }
  if (label !== name) {
    return { title: label, subtitle: "" };
  }
  const areas = ["neighbourhood", "locality", "district", "region", "country"]
    .map((level) => context[level]?.name)
    .filter((area) => area && area !== name);
  return { title: name, subtitle: [...new Set(areas)].join(", ") };
}

// Marker style by how the point relates to the thing: a point on it, a point
// on its street, or the middle of an area.
export function accuracyClass(accuracy) {
  switch (accuracy) {
    case "rooftop":
    case "interpolated":
      return "is-point";
    case "street":
      return "is-street";
    default:
      return "is-area";
  }
}

// A command-list row for a feature: its pin, title and subtitle.
export function featureRowHtml(feature) {
  const { title, subtitle } = featureLines(feature);
  return `
    <span class="command-item-icon result-pin" aria-hidden="true">
      ${icon("map-pin", "command-item-svg")}
    </span>
    <span class="command-item-content">
      <span class="command-item-label">${escapeHtml(title)}</span>
      ${subtitle ? `<span class="command-item-sub">${escapeHtml(subtitle)}</span>` : ""}
    </span>
  `;
}

// Ontario-only demo: lock the map to the province so users can't pan or zoom
// out to areas the pack has no data for.
export const ontarioBounds = [
  [-95.2, 41.6],
  [-74.3, 57.0],
];

let pmtilesProtocolInstalled = false;

function ensurePmtilesProtocol() {
  if (pmtilesProtocolInstalled) return;
  const protocol = new pmtiles.Protocol();
  maplibregl.addProtocol("pmtiles", protocol.tile);
  pmtilesProtocolInstalled = true;
}

// The basemap style: either a hosted, keyless MapLibre style (cfg.styleUrl), or
// an inline style built from a Protomaps PMTiles archive (cfg.pmtilesUrl).
function protomapsStyle() {
  return {
    version: 8,
    glyphs: "https://protomaps.github.io/basemaps-assets/fonts/{fontstack}/{range}.pbf",
    sprite: "https://protomaps.github.io/basemaps-assets/sprites/v4/light",
    sources: {
      protomaps: {
        type: "vector",
        url: `pmtiles://${PMTILES_URL}`,
        attribution:
          'Map data &copy; <a href="https://openstreetmap.org/copyright" target="_blank" rel="noopener">OpenStreetMap</a> contributors &middot; Basemap tiles &copy; <a href="https://protomaps.com" target="_blank" rel="noopener">Protomaps</a>',
      },
    },
    layers: layers("protomaps", namedFlavor("light"), { lang: "en" }),
  };
}

export function createDemoMap(options = {}) {
  ensurePmtilesProtocol();

  const map = new maplibregl.Map({
    container: "map",
    attributionControl: false,
    style: cfg.styleUrl ?? protomapsStyle(),
    center: [-79.3832, 43.6532],
    zoom: 11,
    minZoom: 5,
    maxZoom: 18,
    maxBounds: ontarioBounds,
    ...options,
  });

  map.addControl(new maplibregl.AttributionControl({ compact: true }), "bottom-left");

  // MapLibre renders the compact attribution expanded by default. Collapse it so
  // the demo starts unobtrusive; it still opens on click.
  const attribControl = map.getContainer().querySelector(".maplibregl-ctrl-attrib");
  attribControl?.classList.remove("maplibregl-compact-show");
  attribControl?.removeAttribute("open");

  return map;
}

export function syncZoomControls(map, zoomInButton, zoomOutButton) {
  zoomInButton.disabled = map.getZoom() >= map.getMaxZoom();
  zoomOutButton.disabled = map.getZoom() <= map.getMinZoom();
}

export function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

export function spinner(className = "spinner") {
  return `<svg
    role="status"
    aria-label="Loading"
    class="${className} spinner"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-linecap="round"
    stroke-linejoin="round"
    stroke-width="2"
  >
    <path d="M21 12a9 9 0 1 1-6.219-8.56" />
  </svg>`;
}

export function icon(name, className = "icon") {
  const body = {
    "map-pin": `
      <path d="M20 10c0 4.9-5.1 9.3-7.1 10.8a1.5 1.5 0 0 1-1.8 0C9.1 19.3 4 14.9 4 10a8 8 0 0 1 16 0Z" />
      <circle cx="12" cy="10" r="3" />
    `,
    search: `<path d="m21 21-4.2-4.2m1.2-5.3a6.5 6.5 0 1 1-13 0 6.5 6.5 0 0 1 13 0Z" />`,
    minus: `<path d="M5 12h14" />`,
    plus: `<path d="M5 12h14" /><path d="M12 5v14" />`,
    x: `<path d="M18 6 6 18" /><path d="m6 6 12 12" />`,
  }[name] ?? "";

  return `<svg
    class="${className}"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-linecap="round"
    stroke-linejoin="round"
    stroke-width="2"
    aria-hidden="true"
  >${body}</svg>`;
}
