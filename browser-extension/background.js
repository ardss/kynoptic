// Kynoptic browser extension - background service worker (MV3).
//
// Event-driven only: reports on tabs.onActivated / tabs.onUpdated.
// No polling of page content, no content scripts, no webRequest.
//
// Transport: chrome.runtime.sendNativeMessage to the Kynoptic native host.
// When the host is unreachable (kynoptic not running, host not registered,
// or transient failure), events are buffered in chrome.storage.local and
// flushed in order on the next successful send.

// TODO: replace with the registered native host name once finalized.
const NATIVE_HOST_NAME = "kynoptic_browser_host";

const BUFFER_KEY = "kynoptic_pending_events";
const BUFFER_LIMIT = 500;

// Connection state cached in the worker's global scope; MV3 may kill the
// worker at any time, so state loss is always safe (events go to buffer).
let port = null;
let sending = false;
let flushTimer = null;

// ---------------------------------------------------------------- utilities

function nowMs() {
  return Date.now();
}

function makeEvent(kind, tab, extra) {
  return {
    // event_type matches the kynoptic ingest naming: dot-prefixed sub-kind.
    event_type: ".tab_change",
    ts_ms: nowMs(),
    kind: kind, // "activated" | "updated"
    tab_id: tab ? tab.id : null,
    window_id: tab ? tab.windowId : null,
    url: tab && tab.url ? tab.url : "",
    title: tab && tab.title ? tab.title : "",
    active: tab ? !!tab.active : false,
    ...extra,
  };
}

// ---------------------------------------------------------------- buffering

async function bufferPush(event) {
  const store = await chrome.storage.local.get(BUFFER_KEY);
  const list = Array.isArray(store[BUFFER_KEY]) ? store[BUFFER_KEY] : [];
  list.push(event);
  // Drop oldest when over the limit: buffer is lossy by design, the
  // tab track degrades to "unknown" intervals, it must never block the user.
  const trimmed = list.slice(Math.max(0, list.length - BUFFER_LIMIT));
  await chrome.storage.local.set({ [BUFFER_KEY]: trimmed });
}

async function bufferTake(n) {
  const store = await chrome.storage.local.get(BUFFER_KEY);
  const list = Array.isArray(store[BUFFER_KEY]) ? store[BUFFER_KEY] : [];
  const head = list.slice(0, n);
  await chrome.storage.local.set({ [BUFFER_KEY]: list.slice(n) });
  return head;
}

// ---------------------------------------------------------------- sending

async function sendEvent(event) {
  // sendNativeMessage opens a one-shot connection per call; good enough at
  // event-driven rates and avoids managing port lifetime across worker
  // suspensions.
  return new Promise((resolve) => {
    try {
      chrome.runtime.sendNativeMessage(NATIVE_HOST_NAME, event, (response) => {
        if (chrome.runtime.lastError) {
          resolve({ ok: false, error: chrome.runtime.lastError.message });
        } else {
          resolve({ ok: true, response });
        }
      });
    } catch (e) {
      resolve({ ok: false, error: String(e) });
    }
  });
}

async function flushBuffer() {
  flushTimer = null;
  if (sending) return;
  sending = true;
  try {
    for (;;) {
      const batch = await bufferTake(20);
      if (batch.length === 0) break;
      for (const event of batch) {
        const res = await sendEvent(event);
        if (!res.ok) {
          // Put the failed batch back at the front, preserving order,
          // then stop until the next event wakes us up again.
          const store = await chrome.storage.local.get(BUFFER_KEY);
          const list = Array.isArray(store[BUFFER_KEY]) ? store[BUFFER_KEY] : [];
          await chrome.storage.local.set({ [BUFFER_KEY]: batch.concat(list) });
          return;
        }
      }
    }
  } finally {
    sending = false;
  }
}

async function report(event) {
  const res = await sendEvent(event);
  if (res.ok) {
    // A success may mean the host just came back; drain the backlog.
    if (!flushTimer) flushTimer = setTimeout(flushBuffer, 0);
  } else {
    await bufferPush(event);
    // Retry on the next tab event; MV3 forbids long-lived timers.
    if (!flushTimer) flushTimer = setTimeout(flushBuffer, 30000);
  }
}

// ---------------------------------------------------------------- listeners

chrome.tabs.onActivated.addListener((info) => {
  chrome.tabs.get(info.tabId, (tab) => {
    if (chrome.runtime.lastError || !tab) return;
    report(makeEvent("activated", tab, { previous_tab_id: info.previousTabId ?? null }));
  });
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  // Only meaningful state changes; drop ephemeral favicon/status noise.
  const relevant =
    changeInfo.url !== undefined || changeInfo.title !== undefined;
  if (!relevant || !tab) return;
  report(makeEvent("updated", tab, { change: Object.keys(changeInfo).join(",") }));
});
