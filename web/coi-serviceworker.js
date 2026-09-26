/* Cross-origin isolation shim for static hosts such as GitHub Pages.
 * It injects COOP/COEP into same-origin responses so SharedArrayBuffer-backed
 * WebAssembly threads can start after the first-load reload.
 */
self.addEventListener("install", () => self.skipWaiting());

self.addEventListener("activate", event => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener("fetch", event => {
  const request = event.request;
  if (request.cache === "only-if-cached" && request.mode !== "same-origin") {
    return;
  }

  event.respondWith((async () => {
    const response = await fetch(request);
    if (response.type === "opaque") {
      return response;
    }

    const headers = new Headers(response.headers);
    headers.set("Cross-Origin-Opener-Policy", "same-origin");
    headers.set("Cross-Origin-Embedder-Policy", "require-corp");
    headers.set("Cross-Origin-Resource-Policy", "same-origin");

    return new Response(response.body, {
      status: response.status,
      statusText: response.statusText,
      headers,
    });
  })());
});
