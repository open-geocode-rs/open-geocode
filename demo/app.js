// Protomaps basemap themes — used only by the self-hosted Protomaps style path
// (protomapsStyle below); the OpenFreeMap default never calls it. Loaded from a
// CDN as an ES module (SRI isn't available on bare module imports).
import { layers, namedFlavor } from "https://esm.sh/@protomaps/basemaps@5.7.2";

// The app's mount, derived from the document <base> (index.html). At
// ajha.ca/open-geocode this is "/open-geocode/"; at the root it is "/". A
// root-relative path, fine for fetch().
const BASE = new URL(".", document.baseURI).pathname;

// Public, per-environment config (config.js) selects how this one UI talks to its
// two interchangeable backends; the SAME code below runs for both.
const cfg = window.OPEN_GEOCODE_CONFIG ?? {};

// API base. `open-geocode serve` hosts the UI and the API (/search,
// /autocomplete, /reverse) on the same origin at the root, so config sets
// apiBase:"". When omitted it falls back to <BASE>api, for a mount that serves
// the API under the app's own path.
const API_BASE = cfg.apiBase ?? `${BASE}api`;

// PMTiles URL for the self-hosted Protomaps basemap (deployed demo only); unused
// when config supplies a hosted styleUrl (see the map style below).
const PMTILES_URL = cfg.pmtilesUrl;

// Forward search and autocomplete are the same request flow; they differ
// only in endpoint, the response field, and how results fit the map.
const AUTOCOMPLETE = {
  endpoint: "autocomplete",
  field: "suggestions",
  options: { emptyLabel: "No suggestions", fitMap: false },
};
const FORWARD_SEARCH = { endpoint: "search", field: "results", options: {} };

const searchForm = document.querySelector("#search-form");
const searchIcon = document.querySelector("#search-icon");
const searchInput = document.querySelector("#search-input");
const clearSearch = document.querySelector("#clear-search");
const statusEl = document.querySelector("#status");
const resultsGroup = document.querySelector("#results-group");
const resultsList = document.querySelector("#results");
const reversePanel = document.querySelector("#reverse-panel");
const reverseLabel = document.querySelector("#reverse-label");
const reverseCoords = document.querySelector("#reverse-coords");
const reverseIcon = document.querySelector("#reverse-icon");
const clearReverse = document.querySelector("#clear-reverse");
const zoomInButton = document.querySelector("#zoom-in");
const zoomOutButton = document.querySelector("#zoom-out");

const emptyFeatureCollection = { type: "FeatureCollection", features: [] };
const markerById = new Map();
let searchMarkers = [];
let reverseMarkers = [];
let currentSearch = null;
let currentReverse = null;
let searchDebounce = null;
let collapsedResults = false;
let lastQuery = "";

// Read the PMTiles archive via HTTP range requests.
const protocol = new pmtiles.Protocol();
maplibregl.addProtocol("pmtiles", protocol.tile);

// Ontario-only demo: lock the map to the province so users can't pan or
// zoom out to areas the pack has no data for. Bounds are Ontario's bbox.
const ontarioBounds = [
  [-95.2, 41.6],
  [-74.3, 57.0],
];
// The basemap style — two interchangeable forms, picked by config; the rest of
// this file is identical either way (it adds its own sources/markers ON TOP of
// whichever base style loads):
//   - cfg.styleUrl — a hosted, keyless MapLibre style (this demo uses
//     OpenFreeMap). No key, no account, no self-hosted tiles.
//   - otherwise    — an inline style built from a Protomaps PMTiles archive
//     (cfg.pmtilesUrl); glyphs/sprites come from Protomaps' hosted assets (CDN).
function protomapsStyle() {
  return {
    version: 8,
    // Glyphs + sprites from Protomaps' hosted assets (CDN). They're map data, not
    // code, so nothing is self-hosted here — only the tiles (cfg.pmtilesUrl) are.
    glyphs: "https://protomaps.github.io/basemaps-assets/fonts/{fontstack}/{range}.pbf",
    sprite: "https://protomaps.github.io/basemaps-assets/sprites/v4/light",
    sources: {
      protomaps: {
        type: "vector",
        url: `pmtiles://${PMTILES_URL}`,
        // Resource-qualified: OSM provides the map DATA (required under ODbL —
        // must credit "OpenStreetMap contributors" + link the copyright page);
        // Protomaps builds the basemap TILES/style from it (credited by request).
        attribution:
          'Map data &copy; <a href="https://openstreetmap.org/copyright" target="_blank" rel="noopener">OpenStreetMap</a> contributors &middot; Basemap tiles &copy; <a href="https://protomaps.com" target="_blank" rel="noopener">Protomaps</a>',
      },
    },
    layers: layers("protomaps", namedFlavor("light"), { lang: "en" }),
  };
}

const map = new maplibregl.Map({
  container: "map",
  // We add our own compact AttributionControl below (bottom-left) instead of the
  // default; the brandmark sits bottom-center and the zoom controls bottom-right.
  attributionControl: false,
  style: cfg.styleUrl ?? protomapsStyle(),
  center: [-79.3832, 43.6532],
  zoom: 11,
  minZoom: 5,
  maxZoom: 18,
  maxBounds: ontarioBounds,
});

map.addControl(new maplibregl.AttributionControl({ compact: true }), "bottom-left");

// MapLibre renders the compact attribution EXPANDED by default (it only auto-
// collapses once you drag the map). Collapse it to just the (i) on load so it
// starts unobtrusive; it still opens on click — which OSM's guidelines allow.
// The control is a <details open> element with a toggle class.
const attribControl = map.getContainer().querySelector(".maplibregl-ctrl-attrib");
attribControl?.classList.remove("maplibregl-compact-show");
attribControl?.removeAttribute("open");

map.on("load", () => {
  map.addSource("reverse-line", { type: "geojson", data: emptyFeatureCollection });
  map.addLayer({
    id: "reverse-line",
    type: "line",
    source: "reverse-line",
    layout: { "line-cap": "round" },
    paint: {
      "line-color": "#64748b",
      "line-opacity": 0.42,
      "line-width": 1.5,
      "line-dasharray": [2, 4],
    },
  });
  updateZoomControls();
});

map.on("zoom", updateZoomControls);
map.on("click", (event) => runReverse(event.lngLat));

searchIcon.innerHTML = icon("search");
clearSearch.innerHTML = icon("x");
reverseIcon.innerHTML = icon("map-pin");
clearReverse.innerHTML = icon("x");
zoomInButton.innerHTML = icon("plus");
zoomOutButton.innerHTML = icon("minus");

zoomInButton.addEventListener("click", () => map.zoomIn());
zoomOutButton.addEventListener("click", () => map.zoomOut());
updateZoomControls();

function clearMapResults() {
  for (const marker of searchMarkers) marker.remove();
  searchMarkers = [];
  markerById.clear();
}

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

searchInput.addEventListener("input", () => {
  const query = searchInput.value.trim();
  clearSearch.hidden = query.length === 0;
  window.clearTimeout(searchDebounce);

  if (query.length === 0) {
    clearResults();
    return;
  }

  if (query.length < 2) {
    currentSearch?.abort();
    setSearchLoading(false);
    clearMapResults();
    resultsList.replaceChildren();
    resultsGroup.hidden = true;
    resultsList.hidden = true;
    setStatus("Type at least 2 characters");
    return;
  }

  searchDebounce = window.setTimeout(() => runQuery(query, AUTOCOMPLETE), 220);
});

searchInput.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    window.clearTimeout(searchDebounce);
    currentSearch?.abort();
    setSearchLoading(false);
    searchInput.blur();
  }
});

searchForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const query = searchInput.value.trim();
  clearSearch.hidden = query.length === 0;
  if (query.length === 0) {
    clearResults();
    return;
  }
  window.clearTimeout(searchDebounce);
  runQuery(query, FORWARD_SEARCH);
});

clearSearch.addEventListener("click", () => {
  // First press after selecting a result re-opens the collapsed list
  // (like tapping back into a Google Maps search). A second press clears.
  if (collapsedResults && resultsList.children.length > 0) {
    searchInput.value = lastQuery;
    resultsGroup.hidden = false;
    resultsList.hidden = false;
    collapsedResults = false;
    return;
  }
  searchInput.value = "";
  clearSearch.hidden = true;
  clearResults();
  searchInput.focus();
});

clearReverse.addEventListener("click", clearReverseResult);

document.addEventListener(
  "click",
  (event) => {
    const clearButton = event.target.closest?.("[data-clear-reverse]");
    if (clearButton) {
      event.preventDefault();
      event.stopPropagation();
      clearReverseResult();
      return;
    }

    const popupCloseButton = event.target.closest?.("[data-close-popup]");
    if (popupCloseButton) {
      event.preventDefault();
      event.stopPropagation();
      closeSearchPopups();
    }
  },
  true,
);

async function runQuery(query, mode) {
  lastQuery = query;
  currentSearch?.abort();
  const controller = new AbortController();
  currentSearch = controller;
  setSearchLoading(true);
  resultsGroup.hidden = true;
  resultsList.hidden = true;
  resultsList.replaceChildren();
  clearMapResults();
  setStatus("");

  const params = new URLSearchParams({ q: query, limit: "10" });

  try {
    const response = await fetch(`${API_BASE}/${mode.endpoint}?${params}`, {
      signal: controller.signal,
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error("Search unavailable");
    renderResults(payload[mode.field] ?? [], mode.options);
  } catch (error) {
    if (error.name === "AbortError") return;
    clearMapResults();
    resultsGroup.hidden = true;
    resultsList.hidden = true;
    setStatus("Search unavailable");
  } finally {
    if (currentSearch === controller) {
      currentSearch = null;
      setSearchLoading(false);
    }
  }
}

function renderResults(results, options = {}) {
  const emptyLabel = options.emptyLabel ?? "No results";
  const fitMap = options.fitMap ?? true;
  resultsList.replaceChildren();
  clearMapResults();

  const pointResults = results.filter((result) => result.point);
  if (results.length === 0) {
    resultsGroup.hidden = true;
    resultsList.hidden = true;
    setStatus(emptyLabel);
    return;
  }

  setStatus("");
  resultsGroup.hidden = false;
  resultsList.hidden = false;
  collapsedResults = false;

  for (const result of results) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.className = "command-item";
    button.type = "button";
    button.setAttribute("role", "option");
    button.innerHTML = `
      <span class="command-item-icon result-pin" aria-hidden="true">
        ${icon("map-pin", "command-item-svg")}
      </span>
      <span class="command-item-content">
        <span class="command-item-label">${escapeHtml(result.label)}</span>
      </span>
    `;
    button.addEventListener("click", () => selectResult(result));
    item.appendChild(button);
    resultsList.appendChild(item);

    if (result.point) {
      const marker = makeResultMarker(result).addTo(map);
      searchMarkers.push(marker);
      markerById.set(result.record_id, marker);
    }
  }

  if (!fitMap) {
    return;
  }

  if (pointResults.length === 1) {
    const { lat, lon } = pointResults[0].point;
    map.flyTo({ center: [lon, lat], zoom: Math.max(map.getZoom(), 15), duration: 0.35 * 1000 });
  } else if (pointResults.length > 1) {
    const bounds = new maplibregl.LngLatBounds();
    for (const result of pointResults) {
      bounds.extend([result.point.lon, result.point.lat]);
    }
    map.fitBounds(bounds, { padding: 42, maxZoom: 13, duration: 350 });
  }
}

function makeResultMarker(result) {
  const { lat, lon, precision } = result.point;
  const modifier =
    precision === "representative_point"
      ? "is-representative"
      : precision === "centroid"
        ? "is-centroid"
        : "is-point";
  const element = document.createElement("div");
  element.className = `result-pin-marker ${modifier}`;
  const marker = new maplibregl.Marker({ element }).setLngLat([lon, lat]);
  const popup = new maplibregl.Popup({
    offset: 12,
    closeButton: false,
    maxWidth: "280px",
  }).setHTML(searchPopup(result.label));
  marker.setPopup(popup);
  return marker;
}

async function runReverse(lngLat) {
  currentReverse?.abort();
  const controller = new AbortController();
  currentReverse = controller;
  clearReverseLayers();
  setReverseLoading(lngLat);

  const origin = makeReverseOriginMarker(lngLat).addTo(map);
  reverseMarkers.push(origin);
  const popup = new maplibregl.Popup({
    offset: [0, -28],
    closeButton: false,
    maxWidth: "280px",
  }).setHTML(reversePopup("Searching"));
  origin.setPopup(popup);
  origin.togglePopup();

  const params = new URLSearchParams({
    lon: lngLat.lng.toFixed(7),
    lat: lngLat.lat.toFixed(7),
  });

  try {
    const response = await fetch(`${API_BASE}/reverse?${params}`, {
      signal: controller.signal,
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error("Address unavailable");
    renderReverse(payload, lngLat, origin);
  } catch (error) {
    if (error.name === "AbortError") return;
    renderReverseError("Address unavailable", lngLat, origin);
  } finally {
    if (currentReverse === controller) {
      currentReverse = null;
    }
  }
}

function setReverseLoading(lngLat) {
  reversePanel.hidden = false;
  reverseIcon.innerHTML = spinner("command-item-svg");
  reverseLabel.textContent = "Searching";
  // The clicked coordinates are known immediately and stay shown for the whole
  // lifetime of the result — they never depend on the reverse API succeeding.
  showReverseCoords(lngLat.lat, lngLat.lng);
}

function formatCoords(lat, lon) {
  return `${lat.toFixed(5)}, ${lon.toFixed(5)}`;
}

function showReverseCoords(lat, lon) {
  reverseCoords.textContent = formatCoords(lat, lon);
  reverseCoords.hidden = false;
}

function hideReverseCoords() {
  reverseCoords.textContent = "";
  reverseCoords.hidden = true;
}

function renderReverse(payload, lngLat, origin) {
  const result = payload.result;
  const popup = origin.getPopup();
  // Clicked coordinates are shown regardless of whether an address was found.
  showReverseCoords(lngLat.lat, lngLat.lng);
  if (!result) {
    reversePanel.hidden = false;
    reverseIcon.innerHTML = icon("map-pin", "command-item-svg");
    reverseLabel.textContent = "No address found";
    popup.setHTML(reversePopup("No address found"));
    return;
  }

  reversePanel.hidden = false;
  reverseIcon.innerHTML = icon("map-pin", "command-item-svg");
  reverseLabel.textContent = result.label;
  popup.setHTML(reversePopup(result.label));

  if (result.point) {
    const target = makeReverseTargetMarker(result).addTo(map);
    reverseMarkers.push(target);

    const movedFarEnough =
      Math.abs(result.point.lon - lngLat.lng) > 1e-6 || Math.abs(result.point.lat - lngLat.lat) > 1e-6;
    if (movedFarEnough) {
      map.getSource("reverse-line")?.setData({
        type: "Feature",
        properties: {},
        geometry: {
          type: "LineString",
          coordinates: [
            [lngLat.lng, lngLat.lat],
            [result.point.lon, result.point.lat],
          ],
        },
      });
    }
  }
}

function renderReverseError(message, lngLat, origin) {
  // The address lookup failed, but the click's coordinates are still valid:
  // stop the spinner (swap the loader for the pin), keep the coords in the
  // panel, and show the plain coordinates on the pin instead of "Searching".
  reversePanel.hidden = false;
  reverseIcon.innerHTML = icon("map-pin", "command-item-svg");
  reverseLabel.textContent = message;
  showReverseCoords(lngLat.lat, lngLat.lng);
  origin.getPopup().setHTML(reversePopup(formatCoords(lngLat.lat, lngLat.lng)));
}

function makeReverseOriginMarker(lngLat) {
  const element = document.createElement("div");
  element.className = "reverse-marker";
  element.innerHTML = icon("map-pin", "reverse-marker-svg");
  return new maplibregl.Marker({ element, anchor: "bottom" }).setLngLat(lngLat);
}

function makeReverseTargetMarker(result) {
  const element = document.createElement("div");
  element.className = "reverse-target-marker";
  return new maplibregl.Marker({ element }).setLngLat([result.point.lon, result.point.lat]);
}

function reversePopup(title) {
  return `<div class="popup-body">
    <p class="popup-title">${escapeHtml(title)}</p>
    <button class="popup-close" type="button" aria-label="Clear reverse result" data-clear-reverse>
      ${icon("x")}
    </button>
  </div>`;
}

function searchPopup(title) {
  return `<div class="popup-body">
    <p class="popup-title">${escapeHtml(title)}</p>
    <button class="popup-close" type="button" aria-label="Close popup" data-close-popup>
      ${icon("x")}
    </button>
  </div>`;
}

function selectResult(result) {
  searchInput.value = result.label;
  clearSearch.hidden = false;
  // Collapse the list down to just the selected place; the X button
  // brings the previous list back.
  resultsGroup.hidden = true;
  resultsList.hidden = true;
  collapsedResults = true;
  if (result.point) {
    map.flyTo({
      center: [result.point.lon, result.point.lat],
      zoom: Math.max(map.getZoom(), 16),
      duration: 0.45 * 1000,
    });
  }
  const marker = markerById.get(result.record_id);
  const popup = marker?.getPopup();
  if (marker && popup && !popup.isOpen()) {
    marker.togglePopup();
  }
}

function closeSearchPopups() {
  for (const marker of searchMarkers) {
    const popup = marker.getPopup();
    if (popup && popup.isOpen()) {
      marker.togglePopup();
    }
  }
}

function clearReverseLayers() {
  for (const marker of reverseMarkers) marker.remove();
  reverseMarkers = [];
  map.getSource("reverse-line")?.setData(emptyFeatureCollection);
}

function clearReverseResult() {
  currentReverse?.abort();
  currentReverse = null;
  clearReverseLayers();
  reversePanel.hidden = true;
  reverseLabel.textContent = "";
  hideReverseCoords();
}

function clearResults() {
  window.clearTimeout(searchDebounce);
  currentSearch?.abort();
  setSearchLoading(false);
  clearMapResults();
  resultsList.replaceChildren();
  resultsGroup.hidden = true;
  resultsList.hidden = true;
  collapsedResults = false;
  setStatus("");
}

function setStatus(message) {
  statusEl.textContent = message;
  statusEl.hidden = message.length === 0;
}

function setSearchLoading(isLoading) {
  searchIcon.innerHTML = isLoading ? spinner() : icon("search");
}

function spinner(className = "spinner") {
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

function updateZoomControls() {
  zoomInButton.disabled = map.getZoom() >= map.getMaxZoom();
  zoomOutButton.disabled = map.getZoom() <= map.getMinZoom();
}

function icon(name, className = "icon") {
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
