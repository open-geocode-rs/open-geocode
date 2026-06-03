// open-geocode demo: searches a finished Pack through the Runtime API and shows
// results on an OpenStreetMap basemap (Leaflet). `open-geocode serve` serves both
// this UI and the API (/search, /autocomplete, /reverse) on the same origin, so
// API_BASE is empty. Tiles come from OpenStreetMap (config.js) — no self-hosting.

const cfg = window.OPEN_GEOCODE_CONFIG ?? {};
const API_BASE = cfg.apiBase ?? "";
const TILE_URL = cfg.tileUrl ?? "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
const TILE_ATTRIBUTION =
  cfg.tileAttribution ??
  '&copy; <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a> contributors';

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

let searchMarkers = [];
let reverseMarkers = [];
let reverseLine = null;
let currentSearch = null;
let currentReverse = null;
let searchDebounce = null;
let collapsedResults = false;
let lastQuery = "";

// Ontario-only demo: lock the map to the province so users can't pan or zoom out
// to areas the Pack has no data for. Leaflet bounds are [[south, west], [north, east]].
const ontarioBounds = L.latLngBounds([41.6, -95.2], [57.0, -74.3]);
const map = L.map("map", {
  zoomControl: false, // custom zoom buttons live in .map-controls
  minZoom: 5,
  maxZoom: 18,
  maxBounds: ontarioBounds,
  maxBoundsViscosity: 1,
}).setView([43.6532, -79.3832], 11);

L.tileLayer(TILE_URL, { maxZoom: 19, attribution: TILE_ATTRIBUTION }).addTo(map);
// Attribution bottom-left so it doesn't sit under the bottom-right zoom controls.
map.attributionControl.setPosition("bottomleft");

map.on("zoom", updateZoomControls);
map.on("click", (event) => runReverse(event.latlng));

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

async function runQuery(query, mode) {
  // Forward and reverse search are mutually exclusive in the UI — starting a
  // forward query clears any active reverse result.
  clearReverseResult();

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
    }
  }

  if (!fitMap) {
    return;
  }

  if (pointResults.length === 1) {
    const { lat, lon } = pointResults[0].point;
    map.flyTo([lat, lon], Math.max(map.getZoom(), 15), { duration: 0.35 });
  } else if (pointResults.length > 1) {
    const bounds = L.latLngBounds(pointResults.map((result) => [result.point.lat, result.point.lon]));
    map.fitBounds(bounds, { padding: [42, 42], maxZoom: 13 });
  }
}

function makeResultMarker(result) {
  const { lat, lon } = result.point;
  return L.marker([lat, lon], {
    icon: L.divIcon({
      className: "reverse-marker-divicon",
      html: `<div class="reverse-marker">${icon("map-pin", "reverse-marker-svg")}</div>`,
      iconSize: [32, 32],
      iconAnchor: [16, 32],
    }),
  });
}

async function runReverse(latlng) {
  // Reverse and forward search are mutually exclusive in the UI — starting a
  // reverse lookup clears any active forward search.
  clearForwardSearch();

  currentReverse?.abort();
  const controller = new AbortController();
  currentReverse = controller;
  clearReverseLayers();
  setReverseLoading();

  // Drop the marker immediately; the resolved address shows in the reverse
  // panel under the search bar, so the map just needs the marker.
  const origin = makeReverseOriginMarker(latlng).addTo(map);
  reverseMarkers.push(origin);

  const params = new URLSearchParams({
    lon: latlng.lng.toFixed(7),
    lat: latlng.lat.toFixed(7),
  });

  try {
    const response = await fetch(`${API_BASE}/reverse?${params}`, {
      signal: controller.signal,
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error("Address unavailable");
    renderReverse(payload, latlng);
  } catch (error) {
    if (error.name === "AbortError") return;
    renderReverseError("Address unavailable");
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

function renderReverse(payload, latlng) {
  const result = payload.result;
  if (!result) {
    reversePanel.hidden = false;
    reverseIcon.innerHTML = icon("map-pin", "command-item-svg");
    reverseLabel.textContent = "No address found";
    return;
  }

  reversePanel.hidden = false;
  reverseIcon.innerHTML = icon("map-pin", "command-item-svg");
  reverseLabel.textContent = result.label;

  if (result.point) {
    const target = makeReverseTargetMarker(result).addTo(map);
    reverseMarkers.push(target);

    const movedFarEnough =
      Math.abs(result.point.lon - latlng.lng) > 1e-6 || Math.abs(result.point.lat - latlng.lat) > 1e-6;
    if (movedFarEnough) {
      reverseLine = L.polyline(
        [
          [latlng.lat, latlng.lng],
          [result.point.lat, result.point.lon],
        ],
        { color: "#64748b", weight: 1.5, opacity: 0.42, dashArray: "2 6", lineCap: "round" },
      ).addTo(map);
    }
  }
}

function renderReverseError(message) {
  reversePanel.hidden = false;
  reverseLabel.textContent = message;
}

function makeReverseOriginMarker(latlng) {
  return L.marker([latlng.lat, latlng.lng], {
    icon: L.divIcon({
      className: "reverse-marker-divicon",
      html: `<div class="reverse-marker">${icon("map-pin", "reverse-marker-svg")}</div>`,
      iconSize: [32, 32],
      iconAnchor: [16, 32],
    }),
  });
}

function makeReverseTargetMarker(result) {
  return L.marker([result.point.lat, result.point.lon], {
    icon: L.divIcon({
      className: "reverse-target-divicon",
      html: `<div class="reverse-target-marker"></div>`,
      iconSize: [12, 12],
      iconAnchor: [6, 6],
    }),
  });
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
    map.flyTo([result.point.lat, result.point.lon], Math.max(map.getZoom(), 16), { duration: 0.45 });
  }
}

function clearReverseLayers() {
  for (const marker of reverseMarkers) marker.remove();
  reverseMarkers = [];
  if (reverseLine) {
    reverseLine.remove();
    reverseLine = null;
  }
}

function clearReverseResult() {
  currentReverse?.abort();
  currentReverse = null;
  clearReverseLayers();
  reversePanel.hidden = true;
  reverseLabel.textContent = "";
}

function clearForwardSearch() {
  searchInput.value = "";
  clearSearch.hidden = true;
  clearResults();
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
