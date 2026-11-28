// Lets tom-os be installed ("Add to Home screen") and start without a
// connection. Files are taken from the network when it answers, so a new
// version is picked up at once, and from the last copy otherwise.
const CACHE = "tomos-1";

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== "GET" || url.origin !== self.location.origin) return; // LLM requests pass untouched
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    try {
      const response = await fetch(event.request);
      if (response.ok) cache.put(event.request, response.clone());
      return response;
    } catch (error) {
      const saved = await cache.match(event.request, { ignoreSearch: true });
      if (saved) return saved;
      throw error;
    }
  })());
});
