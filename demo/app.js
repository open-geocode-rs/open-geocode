import { layers, namedFlavor } from "/vendor/basemaps.js";

// Configurable endpoints. Local dev keeps everything same-origin; when the
// demo is split across Cloudflare Pages + an API host, point API_BASE at the
// API origin and PMTILES_URL at the R2-hosted archive.
const API_BASE = "";
const PMTILES_URL = `${location.origin}/basemap.pmtiles`;

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
const map = new maplibregl.Map({
  container: "map",
  // Add attribution to the bottom-left so it doesn't sit under the bottom-right
  // zoom controls.
  attributionControl: false,
  style: {
    version: 8,
    glyphs: "/vendor/fonts/{fontstack}/{range}.pbf",
    sprite: "/vendor/sprites/v4/light",
    sources: {
      protomaps: {
        type: "vector",
        url: `pmtiles://${PMTILES_URL}`,
        attribution:
          '&copy; <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>',
      },
    },
    layers: layers("protomaps", namedFlavor("light"), { lang: "en" }),
  },
  center: [-79.3832, 43.6532],
  zoom: 11,
  minZoom: 5,
  maxZoom: 18,
  maxBounds: ontarioBounds,
});

map.addControl(new maplibregl.AttributionControl({ compact: true }), "bottom-left");

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
  setReverseLoading();

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
    renderReverseError("Address unavailable", origin);
  } finally {
    if (currentReverse === controller) {
      currentReverse = null;
    }
  }
}

function setReverseLoading() {
  reversePanel.hidden = false;
  reverseIcon.innerHTML = spinner("command-item-svg");
  reverseLabel.textContent = "Searching";
}

function renderReverse(payload, lngLat, origin) {
  const result = payload.result;
  const popup = origin.getPopup();
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

function renderReverseError(message, origin) {
  reversePanel.hidden = false;
  reverseLabel.textContent = message;
  origin.getPopup().setHTML(reversePopup(message));
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
