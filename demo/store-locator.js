import { API_BASE, createDemoMap, escapeHtml, icon, spinner, syncZoomControls } from "./shared.js";

const AUTOCOMPLETE = {
  endpoint: "autocomplete",
  field: "suggestions",
  emptyLabel: "No address suggestions",
};
const FORWARD_SEARCH = {
  endpoint: "search",
  field: "results",
  emptyLabel: "No address results",
};

const stores = [
  {
    id: "harbourfront",
    name: "Harbourfront Pharmacy",
    category: "Pharmacy",
    address: "Queens Quay W, Toronto",
    distance: "0.4 km",
    status: "Open",
    hours: "Closes 7 p.m.",
    phone: "(416) 607-5552",
    services: ["Prescriptions", "Vaccines"],
    point: { lat: 43.6409, lon: -79.3812 },
  },
  {
    id: "cityplace",
    name: "CityPlace Pharmacy",
    category: "Pharmacy",
    address: "Lower Simcoe St, Toronto",
    distance: "0.7 km",
    status: "Open",
    hours: "Closes 7 p.m.",
    phone: "(416) 214-2489",
    services: ["Compounding", "Consultations"],
    point: { lat: 43.6429, lon: -79.3942 },
  },
  {
    id: "bathurst-quay",
    name: "Bathurst Quay Pharmacy",
    category: "Pharmacy",
    address: "Bathurst Quay, Toronto",
    distance: "1.1 km",
    status: "Open",
    hours: "Closes 8 p.m.",
    phone: "(416) 977-0970",
    services: ["Prescriptions", "Travel health"],
    point: { lat: 43.6359, lon: -79.3988 },
  },
  {
    id: "fort-york",
    name: "Fort York Pharmacy",
    category: "Pharmacy",
    address: "Fort York Blvd, Toronto",
    distance: "1.5 km",
    status: "Open",
    hours: "Closes 6 p.m.",
    phone: "(416) 555-0184",
    services: ["Vaccines", "Medication checks"],
    point: { lat: 43.6389, lon: -79.407 },
  },
  {
    id: "st-lawrence",
    name: "St. Lawrence Pharmacy",
    category: "Pharmacy",
    address: "Yonge St, Toronto",
    distance: "1.8 km",
    status: "Open",
    hours: "Closes 7 p.m.",
    phone: "(416) 555-0137",
    services: ["Prescriptions", "Refills"],
    point: { lat: 43.6474, lon: -79.3727 },
  },
];

const locatorForm = document.querySelector("#locator-form");
const locatorIcon = document.querySelector("#locator-icon");
const locatorInput = document.querySelector("#locator-input");
const clearLocator = document.querySelector("#clear-locator");
const statusEl = document.querySelector("#locator-status");
const resultsGroup = document.querySelector("#store-results-group");
const resultsList = document.querySelector("#store-results");
const zoomInButton = document.querySelector("#zoom-in");
const zoomOutButton = document.querySelector("#zoom-out");

const map = createDemoMap({
  center: [-79.389, 43.641],
  zoom: 13,
});

const markerById = new Map();
let currentAddressSearch = null;
let addressDebounce = null;
let originMarker = null;
let selectedAddress = null;
let selectedStoreId = null;

locatorIcon.innerHTML = icon("search");
clearLocator.innerHTML = icon("x");
zoomInButton.innerHTML = icon("plus");
zoomOutButton.innerHTML = icon("minus");

zoomInButton.addEventListener("click", () => map.zoomIn());
zoomOutButton.addEventListener("click", () => map.zoomOut());

locatorForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const query = locatorInput.value.trim();
  clearLocator.hidden = query.length === 0;
  if (query.length === 0) {
    clearLocatorState();
    clearResults();
    return;
  }
  window.clearTimeout(addressDebounce);
  runAddressQuery(query, FORWARD_SEARCH);
});

locatorInput.addEventListener("input", () => {
  const query = locatorInput.value.trim();
  clearLocator.hidden = query.length === 0;
  clearLocatorState();
  window.clearTimeout(addressDebounce);

  if (query.length === 0) {
    clearResults();
    return;
  }

  if (query.length < 2) {
    currentAddressSearch?.abort();
    setAddressLoading(false);
    resultsList.replaceChildren();
    resultsGroup.hidden = true;
    setStatus("Type at least 2 characters");
    return;
  }

  addressDebounce = window.setTimeout(() => runAddressQuery(query, AUTOCOMPLETE), 220);
});

locatorInput.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    window.clearTimeout(addressDebounce);
    currentAddressSearch?.abort();
    setAddressLoading(false);
    locatorInput.blur();
  }
});

clearLocator.addEventListener("click", () => {
  locatorInput.value = "";
  clearLocator.hidden = true;
  window.clearTimeout(addressDebounce);
  currentAddressSearch?.abort();
  setAddressLoading(false);
  clearLocatorState();
  clearResults();
  locatorInput.focus();
});

document.addEventListener(
  "click",
  (event) => {
    const popupCloseButton = event.target.closest?.("[data-close-popup]");
    if (!popupCloseButton) return;
    event.preventDefault();
    event.stopPropagation();
    closeOriginPopup();
    closeStorePopups();
  },
  true,
);

map.on("load", () => {
  clearResults();
  updateZoomControls();
});

map.on("zoom", updateZoomControls);
updateZoomControls();

async function runAddressQuery(query, mode) {
  currentAddressSearch?.abort();
  const controller = new AbortController();
  currentAddressSearch = controller;
  setAddressLoading(true);
  resultsList.replaceChildren();
  resultsGroup.hidden = true;
  setStatus("");

  const params = new URLSearchParams({ q: query, limit: "10" });
  try {
    const response = await fetch(`${API_BASE}/${mode.endpoint}?${params}`, {
      signal: controller.signal,
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error("Address search unavailable");
    renderAddressSuggestions(payload[mode.field] ?? [], mode.emptyLabel);
  } catch (error) {
    if (error.name === "AbortError") return;
    resultsList.replaceChildren();
    resultsGroup.hidden = true;
    setStatus("Address search unavailable");
  } finally {
    if (currentAddressSearch === controller) {
      currentAddressSearch = null;
      setAddressLoading(false);
    }
  }
}

function renderAddressSuggestions(results, emptyLabel) {
  const pointResults = results.filter((result) => result.point);
  resultsList.replaceChildren();

  if (pointResults.length === 0) {
    resultsGroup.hidden = true;
    setStatus(emptyLabel);
    return;
  }

  setStatus("");
  resultsGroup.hidden = false;
  for (const result of pointResults) {
    resultsList.appendChild(makeAddressRow(result));
  }
}

function makeAddressRow(address) {
  const item = document.createElement("li");
  const button = document.createElement("button");
  button.className = "command-item address-item";
  button.type = "button";
  button.setAttribute("role", "option");
  button.innerHTML = `
    <span class="command-item-icon result-pin" aria-hidden="true">
      ${icon("map-pin", "command-item-svg")}
    </span>
    <span class="command-item-content">
      <span class="command-item-label">${escapeHtml(address.label)}</span>
    </span>
  `;
  button.addEventListener("click", () => selectAddress(address));
  item.appendChild(button);
  return item;
}

function selectAddress(address) {
  if (!address.point) return;

  window.clearTimeout(addressDebounce);
  currentAddressSearch?.abort();
  setAddressLoading(false);
  selectedAddress = {
    label: address.label,
    point: address.point,
  };
  selectedStoreId = null;
  locatorInput.value = address.label;
  clearLocator.hidden = false;
  renderStores(stores, selectedAddress);
  fitStores(stores, 350);
}

function renderStores(matches, address) {
  closeStorePopups();
  clearMarkers();
  resultsList.replaceChildren();
  clearOriginMarker();

  if (!matches.some((store) => store.id === selectedStoreId)) {
    selectedStoreId = null;
  }

  if (matches.length === 0) {
    resultsGroup.hidden = true;
    setStatus("No nearby pharmacies found");
    return;
  }

  setStatus(`Nearby pharmacies for ${address.label}`);
  resultsGroup.hidden = false;
  originMarker = makeOriginMarker(address).addTo(map);
  originMarker.togglePopup();

  for (const store of matches) {
    resultsList.appendChild(makeStoreRow(store));
    const marker = makeStoreMarker(store).addTo(map);
    markerById.set(store.id, marker);
  }

  syncSelectedState();
}

function makeStoreRow(store) {
  const item = document.createElement("li");
  const button = document.createElement("button");
  button.className = "command-item store-item";
  button.type = "button";
  button.setAttribute("role", "option");
  button.dataset.storeId = store.id;
  button.innerHTML = `
    <span class="command-item-icon store-pin" aria-hidden="true">
      ${icon("map-pin", "command-item-svg")}
    </span>
    <span class="command-item-content">
      <span class="command-item-label">${escapeHtml(store.name)}</span>
      <span class="command-item-sub">${escapeHtml(store.distance)} &middot; ${escapeHtml(
        store.category,
      )} &middot; ${escapeHtml(store.address)}</span>
      <span class="command-item-sub">
        <span class="store-open">${escapeHtml(store.status)}</span>
        &middot; ${escapeHtml(store.hours)} &middot; ${escapeHtml(store.phone)}
      </span>
      <span class="command-item-sub">${escapeHtml(store.services.join(" · "))}</span>
    </span>
  `;
  button.addEventListener("click", () => selectStore(store.id));
  item.appendChild(button);
  return item;
}

function makeOriginMarker(address) {
  const element = document.createElement("div");
  element.className = "locator-origin-marker";
  const marker = new maplibregl.Marker({ element }).setLngLat([address.point.lon, address.point.lat]);
  const popup = new maplibregl.Popup({
    offset: 12,
    closeButton: false,
    maxWidth: "280px",
  }).setHTML(addressPopup(address));
  marker.setPopup(popup);
  return marker;
}

function makeStoreMarker(store) {
  const element = document.createElement("div");
  element.className = "store-pin-marker";
  element.innerHTML = icon("map-pin", "store-pin-marker-svg");

  const marker = new maplibregl.Marker({ element, anchor: "bottom" }).setLngLat([
    store.point.lon,
    store.point.lat,
  ]);
  const popup = new maplibregl.Popup({
    offset: [0, -30],
    closeButton: false,
    maxWidth: "280px",
  }).setHTML(storePopup(store));
  marker.setPopup(popup);
  element.addEventListener("click", () => selectStore(store.id, { fly: false }));
  return marker;
}

function selectStore(storeId, options = {}) {
  const store = stores.find((candidate) => candidate.id === storeId);
  const marker = markerById.get(storeId);
  if (!store || !marker) return;

  selectedStoreId = storeId;
  syncSelectedState();

  if (options.fly !== false) {
    map.flyTo({
      center: [store.point.lon, store.point.lat],
      zoom: Math.max(map.getZoom(), 15),
      duration: 350,
    });
  }

  const popup = marker.getPopup();
  if (popup && !popup.isOpen()) {
    marker.togglePopup();
  }
}

function syncSelectedState() {
  for (const button of resultsList.querySelectorAll("[data-store-id]")) {
    const isSelected = button.dataset.storeId === selectedStoreId;
    button.setAttribute("aria-selected", String(isSelected));
  }

  for (const [storeId, marker] of markerById) {
    marker.getElement().classList.toggle("is-selected", storeId === selectedStoreId);
  }
}

function fitStores(matches, duration) {
  if (matches.length === 0 || !selectedAddress || !map.loaded()) return;

  if (matches.length === 1) {
    const store = matches[0];
    map.flyTo({
      center: [store.point.lon, store.point.lat],
      zoom: Math.max(map.getZoom(), 15),
      duration,
    });
    return;
  }

  const bounds = new maplibregl.LngLatBounds();
  bounds.extend([selectedAddress.point.lon, selectedAddress.point.lat]);
  for (const store of matches) {
    bounds.extend([store.point.lon, store.point.lat]);
  }

  const widePanel = window.matchMedia("(min-width: 700px)").matches;
  map.fitBounds(bounds, {
    padding: widePanel ? { top: 84, right: 84, bottom: 84, left: 500 } : 64,
    maxZoom: 14,
    duration,
  });
}

function clearMarkers() {
  for (const marker of markerById.values()) marker.remove();
  markerById.clear();
}

function clearLocatorState() {
  selectedAddress = null;
  selectedStoreId = null;
  closeOriginPopup();
  clearOriginMarker();
  closeStorePopups();
  clearMarkers();
}

function clearOriginMarker() {
  originMarker?.remove();
  originMarker = null;
}

function clearResults() {
  resultsList.replaceChildren();
  resultsGroup.hidden = true;
  setStatus("Enter an address to find nearby pharmacies");
}

function closeOriginPopup() {
  const popup = originMarker?.getPopup();
  if (popup?.isOpen()) {
    originMarker.togglePopup();
  }
}

function closeStorePopups() {
  for (const marker of markerById.values()) {
    const popup = marker.getPopup();
    if (popup?.isOpen()) {
      marker.togglePopup();
    }
  }
}

function storePopup(store) {
  return `<div class="popup-body">
    <p class="popup-title">
      <span class="popup-title-main">${escapeHtml(store.name)}</span>
      <span class="popup-title-sub">${escapeHtml(store.address)}</span>
    </p>
    <button class="popup-close" type="button" aria-label="Close popup" data-close-popup>
      ${icon("x")}
    </button>
  </div>`;
}

function addressPopup(address) {
  return `<div class="popup-body">
    <p class="popup-title">
      <span class="popup-title-main">${escapeHtml(address.label)}</span>
      <span class="popup-title-sub">${escapeHtml(formatCoords(address.point.lat, address.point.lon))}</span>
    </p>
    <button class="popup-close" type="button" aria-label="Close popup" data-close-popup>
      ${icon("x")}
    </button>
  </div>`;
}

function formatCoords(lat, lon) {
  return `${lat.toFixed(5)}, ${lon.toFixed(5)}`;
}

function setStatus(message) {
  statusEl.textContent = message;
  statusEl.hidden = message.length === 0;
}

function setAddressLoading(isLoading) {
  locatorIcon.innerHTML = isLoading ? spinner() : icon("search");
}

function updateZoomControls() {
  syncZoomControls(map, zoomInButton, zoomOutButton);
}
