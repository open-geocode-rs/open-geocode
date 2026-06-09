import {
  API_BASE,
  createDemoMap,
  emptyFeatureCollection,
  escapeHtml,
  icon,
  spinner,
  syncZoomControls,
} from "./shared.js";

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

const markerById = new Map();
let searchMarkers = [];
let reverseMarkers = [];
let currentSearch = null;
let currentReverse = null;
let searchDebounce = null;
let collapsedResults = false;
let lastQuery = "";

const map = createDemoMap();

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

function updateZoomControls() {
  syncZoomControls(map, zoomInButton, zoomOutButton);
}
