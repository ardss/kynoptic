// Kynoptic browser extension - ChatGPT web session pairing in the service
// worker (F03). The content script reports raw start/end signals; this file
// pairs them per tab and emits one .ai_session event per session through the
// existing report() / native messaging path.
//
// Orphan policy (append-only, no guessing): a start without a matching end
// (browser killed, worker suspended) is dropped - the timeline keeps that
// gap as unknown instead of fabricating an end time.

"use strict";

const PENDING_KEY = "kynoptic_chatgpt_pending";
const CHATGPT_ORIGINS = ["https://chatgpt.com/*", "https://chat.openai.com/*"];

function rfc3339(ms) {
  return new Date(ms).toISOString();
}

async function pendingGet() {
  const store = await chrome.storage.local.get(PENDING_KEY);
  return Array.isArray(store[PENDING_KEY]) ? store[PENDING_KEY] : [];
}

async function pendingSet(list) {
  await chrome.storage.local.set({ [PENDING_KEY]: list });
}

function makeAiSessionEvent(startMs, endMs, tabId) {
  return {
    event_type: ".ai_session",
    ts_ms: Date.now(),
    schema_version: 1,
    event_data: {
      schema_version: 1,
      session_id: `chatgpt_web-tab${tabId ?? "?"}-${startMs}`,
      source: "chatgpt_web",
      start: rfc3339(startMs),
      end: rfc3339(endMs),
      // Both ends are direct page-lifecycle evidence, not inference.
      start_quality: "observed",
      end_quality: "observed",
      prompt_count: null,
    },
  };
}

// Returns true when the message actually came from a ChatGPT page.
function isChatgptSender(sender) {
  const url = sender && sender.url ? new URL(sender.url) : null;
  if (!url) return false;
  return (
    url.hostname === "chatgpt.com" ||
    url.hostname === "chat.openai.com" ||
    url.hostname.endsWith(".chatgpt.com")
  );
}

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (!msg || typeof msg.type !== "string" || !msg.type.startsWith("kynoptic_chatgpt_")) {
    return; // not ours; let other listeners handle it
  }
  sendResponse({ ok: true });
  if (!isChatgptSender(sender)) return; // only trust ChatGPT page origins

  (async () => {
    const tabId = sender.tab ? sender.tab.id : null;
    const list = await pendingGet();
    const ts = typeof msg.ts_ms === "number" ? msg.ts_ms : Date.now();
    if (msg.type === "kynoptic_chatgpt_session_start") {
      // One open session per tab: a new start on the same tab supersedes the
      // previous unpaired start (that one had no end evidence - dropped, not guessed).
      const filtered = list.filter((p) => p.tab_id !== tabId);
      filtered.push({ tab_id: tabId, start_ms: ts });
      await pendingSet(filtered);
    } else if (msg.type === "kynoptic_chatgpt_session_end") {
      const idx = list.findIndex((p) => p.tab_id === tabId);
      if (idx === -1) return; // end without start (worker restart) - nothing to fabricate
      const start = list.splice(idx, 1)[0];
      await pendingSet(list);
      if (ts > start.start_ms) {
        report(makeAiSessionEvent(start.start_ms, ts, tabId));
      }
    }
  })();
  return false;
});

// MV3 may kill the worker; pending starts survive in storage but become
// unverifiable after a restart window. On startup, drop orphans honestly
// (their end was never observed).
chrome.runtime.onStartup.addListener(() => {
  pendingSet([]);
});
