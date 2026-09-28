"use strict";
// Remonte les erreurs JS au serveur local (visibles dans le terminal).
  window.__log = (m) => { try { navigator.sendBeacon("/api/log", String(m)); } catch (e) {} };
  addEventListener("error", (e) => window.__log(`${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`));
  addEventListener("unhandledrejection", (e) => window.__log("promise: " + (e.reason && (e.reason.stack || e.reason))));
