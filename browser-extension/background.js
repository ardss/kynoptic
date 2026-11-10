// Kynoptic browser extension - background service worker (MV3).
//
// Event-driven only: reports on tabs.onActivated / tabs.onUpdated.
// No polling of page content, no webRequest.
//
// F03: the ChatGPT web session collector (content-chatgpt.js, opt-in via the
// popup) sends start/end signals here; chatgpt-session.js pairs them per tab
// and hands this worker one .ai_session event, which goes out through the
// same report() / native messaging path as tab events.
//
// Transport: chrome.runtime.sendNativeMessage to the Kynoptic native host.
// When the host is unreachable (kynoptic not running, host not registered,
// or transient failure), events are buffered in chrome.storage.local and
// flushed in order on the next successful send.
importScripts("chatgpt-session.js");

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

// Review 2026-11-11 (low): `${ms}-${kind}-${tabId}` alone can collide when
// the same tab produces two same-kind events within one millisecond (rapid
// onUpdated url/title churn, adjacent buffered replay entries). Append a
// per-worker sequence number plus a random worker-instance seed: the
// sequence disambiguates within one worker lifetime, the seed disambiguates
// across MV3 service-worker cold starts. Buffered replay resends carry the
// SAME stored event object, so ids stay stable across resends and the
// native host dedup still works.
const EVENT_ID_SEED = Math.random().toString(36).slice(2, 8);
let eventSeq = 0;
function nextEventSeq() {
  eventSeq = (eventSeq + 1) % 0xffff;
  return eventSeq.toString(36);
}

function makeEvent(kind, tab, extra) {
  return {
    // event_type matches the kynoptic ingest naming: dot-prefixed sub-kind.
    event_type: ".tab_change",
    // Client-side idempotency key: the native host dedupes on this
    // (json_extract(event_data,'$.event_id')) when buffered events are
    // replayed after a reconnect, so a resend never produces a second row.
    event_id: `${nowMs()}-${kind}-${tab ? tab.id : "?"}-${EVENT_ID_SEED}${nextEventSeq()}`,
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
