// Lightweight open-geocode demo: Leaflet + OpenStreetMap tiles, talking to the
// Runtime's own API on the same origin. `open-geocode serve` serves both this UI
// and /search, /autocomplete, /reverse — so there is no Worker and no /api
// prefix here. Plain JS, no build step.

const cfg = window.OPEN_GEOCODE_CONFIG ?? {};
const API = cfg.apiBase ?? "";
const TILE_URL = cfg.tileUrl ?? "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
const TILE_ATTRIBUTION =
  cfg.tileAttribution ??
  '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors';

// Default view: Toronto. If tiles fail to load the map still works — markers
// just render on a blank background.
const map = L.map("map").setView([43.6532, -79.3832], 11);
L.tileLayer(TILE_URL, { maxZoom: 19, attribution: TILE_ATTRIBUTION }).addTo(map);

const resultLayer = L.layerGroup().addTo(map);
let reverseMarker = null;
let searchAbort = null;
let debounce = null;

const input = document.querySelector("#q");
const list = document.querySelector("#results");
const statusEl = document.querySelector("#status");

function setStatus(message) {
  statusEl.textContent = message ?? "";
  statusEl.hidden = !message;
}

function clearResults() {
  resultLayer.clearLayers();
  list.replaceChildren();
}

function renderResults(items) {
  list.replaceChildren();
  const points = [];
  for (const item of items) {
    const li = document.createElement("li");
    li.className = "result";
    li.textContent = item.label;
    if (item.point) {
      const latlng = [item.point.lat, item.point.lon];
      const marker = L.marker(latlng).addTo(resultLayer).bindPopup(item.label);
      points.push(latlng);
      li.addEventListener("click", () => {
        map.flyTo(latlng, Math.max(map.getZoom(), 16));
        marker.openPopup();
      });
    }
    list.appendChild(li);
  }
  if (points.length === 1) map.flyTo(points[0], Math.max(map.getZoom(), 15));
  else if (points.length > 1) map.fitBounds(points, { padding: [40, 40], maxZoom: 14 });
}

// `endpoint`/`field` switch between live autocomplete and a full search submit.
async function runSearch(query, endpoint, field) {
  searchAbort?.abort();
  clearResults();
  if (!query) return setStatus("");
  const controller = new AbortController();
  searchAbort = controller;
  setStatus("Searching…");
  try {
    const qs = new URLSearchParams({ q: query, limit: "10" });
    const res = await fetch(`${API}/${endpoint}?${qs}`, { signal: controller.signal });
    if (!res.ok) throw new Error(`${endpoint} ${res.status}`);
    const items = (await res.json())[field] ?? [];
    if (!items.length) return setStatus("No results");
    setStatus("");
    renderResults(items);
  } catch (err) {
    if (err.name !== "AbortError") setStatus("Search unavailable");
  }
}

async function runReverse(lat, lng) {
  reverseMarker?.remove();
  reverseMarker = L.marker([lat, lng]).addTo(map).bindPopup("Looking up…").openPopup();
  try {
    const qs = new URLSearchParams({ lon: lng.toFixed(7), lat: lat.toFixed(7) });
    const res = await fetch(`${API}/reverse?${qs}`);
    if (!res.ok) throw new Error(`reverse ${res.status}`);
    const result = (await res.json()).result;
    reverseMarker.setPopupContent(result?.label ?? "No address found").openPopup();
  } catch {
    reverseMarker.setPopupContent("Address unavailable").openPopup();
  }
}

input.addEventListener("input", () => {
  const query = input.value.trim();
  clearTimeout(debounce);
  if (query.length < 2) {
    clearResults();
    return setStatus(query ? "Keep typing…" : "");
  }
  debounce = setTimeout(() => runSearch(query, "autocomplete", "suggestions"), 200);
});

document.querySelector("#form").addEventListener("submit", (event) => {
  event.preventDefault();
  clearTimeout(debounce);
  runSearch(input.value.trim(), "search", "results");
});

map.on("click", (event) => runReverse(event.latlng.lat, event.latlng.lng));
