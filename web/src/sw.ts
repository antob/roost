/// <reference lib="webworker" />
declare const self: ServiceWorkerGlobalScope;

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

self.addEventListener("push", (event) => {
  const data = event.data?.json() ?? { title: "roost", body: "" };
  const shown = self.registration.showNotification(data.title ?? "roost", {
    body: data.channel ? `${data.channel} · ${data.body ?? ""}`.trim() : data.body,
    tag: data.tag ?? (data.id != null ? `n-${data.id}` : undefined),
    data: { url: data.url ?? "/", id: data.id },
    icon: "/icon-192.png",
    badge: "/icon-192.png",
    requireInteraction: data.sticky ?? data.profile === "urgent",
  } as NotificationOptions);
  // The receipt lets the agent see `delivered`; iOS revokes subscriptions that skip showNotification.
  const receipt =
    data.id != null
      ? fetch(`/api/human/notifications/${data.id}/delivered`, { method: "POST", credentials: "same-origin" }).catch(() => {})
      : Promise.resolve();
  const tell = self.clients
    .matchAll({ type: "window" })
    .then((cs) => cs.forEach((c) => c.postMessage({ type: "push", id: data.id })));
  const badge =
    typeof data.badge === "number" && "setAppBadge" in self.navigator
      ? (data.badge > 0 ? self.navigator.setAppBadge(data.badge) : self.navigator.clearAppBadge()).catch(() => {})
      : Promise.resolve();
  event.waitUntil(Promise.all([shown, receipt, tell, badge]));
});

// iOS may drop the postMessage to a waking client and may open start_url instead of
// openWindow's URL, so the target is also parked in the cache for the page to pick up.
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const url: string = event.notification.data?.url ?? "/";
  event.waitUntil(
    (async () => {
      const cache = await caches.open("roost-open");
      await cache.put("/__open", new Response(JSON.stringify({ url, at: Date.now() })));
      const clients = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      const client = clients[0];
      if (!client) {
        await self.clients.openWindow(url).catch(() => {});
        return;
      }
      await Promise.race([client.focus().catch(() => {}), new Promise((r) => setTimeout(r, 2000))]);
      client.postMessage({ type: "open", url });
    })(),
  );
});
