// Kynoptic browser extension - ChatGPT web session content script (F03).
//
// Opt-in only: this file is registered via chrome.scripting from the popup
// (never in the manifest), and it re-checks the stored flag on every
// activation. When the flag is off it does nothing.
//
// What it reports: session start/end only. It never reads page text, chat
// content, titles or user prompts. It only observes:
//   - whether the current route is a conversation route (/c/... or /g/...),
//   - document visibility changes,
//   - page unload.
//
// Transport: chrome.runtime.sendMessage to the service worker, which pairs
// start/end and emits one .ai_session event per session over the existing
// native messaging channel.

"use strict";

const FLAG_KEY = "kynoptic_chatgpt_enabled";

function enabled() {
  return chrome.storage.local.get(FLAG_KEY).then((store) => store[FLAG_KEY] === true);
}

// A conversation route = the user is inside a specific chat thread.
// New-chat home pages ("/") do not count as an active session.
function inConversationRoute() {
  return /^\/(c|g)\//.test(location.pathname);
}

let sessionActive = false;

function send(kind) {
  // No page content here - only a kind and a timestamp. The isolated world
  // cannot be fingerprinted by the page for reading these.
  try {
    chrome.runtime.sendMessage({ type: kind, ts_ms: Date.now() });
  } catch (e) {
    // Extension context invalidated (update/reload): drop silently, the
    // in-flight session ends unpaired and stays unknown on the timeline.
  }
}

function maybeStart(reason) {
  if (sessionActive) return;
  if (!inConversationRoute()) return;
  if (document.visibilityState !== "visible") return;
  sessionActive = true;
  send("kynoptic_chatgpt_session_start");
}

function endSession(reason) {
  if (!sessionActive) return;
  sessionActive = false;
  send("kynoptic_chatgpt_session_end");
}

// SPA route change: chatgpt.com rewrites history without navigation.
// Patch history.pushState/replaceState instead of polling the page.
const _push = history.pushState.bind(history);
const _replace = history.replaceState.bind(history);
history.pushState = function (...args) {
  _push(...args);
  onRouteChange();
};
history.replaceState = function (...args) {
  _replace(...args);
  onRouteChange();
};
window.addEventListener("popstate", onRouteChange);

function onRouteChange() {
  if (inConversationRoute()) {
    maybeStart("route");
  } else {
    endSession("route");
  }
}

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") {
    maybeStart("visible");
  } else {
    endSession("hidden");
  }
});

window.addEventListener("pagehide", () => endSession("pagehide"));

(async function init() {
  if (!(await enabled())) return; // opt-in gate (registration is the first gate)
  maybeStart("load");
})();
