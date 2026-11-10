// Kynoptic browser extension - ChatGPT web opt-in toggle (F03).
// Off by default. Turning it on registers the ChatGPT content script via
// chrome.scripting (persisted across sessions); turning it off unregisters
// it immediately and clears any unpaired session start.

"use strict";

const FLAG_KEY = "kynoptic_chatgpt_enabled";
const SCRIPT_ID = "kynoptic-chatgpt-session";
const MATCHES = ["https://chatgpt.com/*", "https://chat.openai.com/*"];
const PENDING_KEY = "kynoptic_chatgpt_pending";

const toggle = document.getElementById("chatgpt-toggle");
const statusEl = document.getElementById("status");

function setStatus(text) {
  statusEl.textContent = text;
}

async function refresh() {
  const store = await chrome.storage.local.get(FLAG_KEY);
  toggle.checked = store[FLAG_KEY] === true;
  const scripts = await chrome.scripting.getRegisteredContentScripts();
  const registered = scripts.some((s) => s.id === SCRIPT_ID);
  if (toggle.checked && !registered) {
    // Flag is on but registration is missing (e.g. extension reload):
    // self-heal by re-registering.
    await registerScript();
    setStatus("已开启（重新注册 content script）");
    return;
  }
  setStatus(toggle.checked ? "已开启：ChatGPT 网页会话采集运行中" : "已关闭：不采集任何 ChatGPT 网页数据");
}

async function registerScript() {
  await chrome.scripting.registerContentScripts([
    {
      id: SCRIPT_ID,
      matches: MATCHES,
      js: ["content-chatgpt.js"],
      runAt: "document_idle",
      persistAcrossSessions: true,
    },
  ]);
}

async function unregisterScript() {
  await chrome.scripting.unregisterContentScripts({ ids: [SCRIPT_ID] });
}

toggle.addEventListener("change", async () => {
  try {
    if (toggle.checked) {
      await chrome.storage.local.set({ [FLAG_KEY]: true });
      await registerScript();
      setStatus("已开启：ChatGPT 网页会话采集运行中（重新打开 ChatGPT 标签页生效）");
    } else {
      await chrome.storage.local.set({ [FLAG_KEY]: false });
      await unregisterScript().catch(() => {});
      // Drop the unpaired start: the session end will never come from a
      // disabled source, and we do not fabricate one.
      await chrome.storage.local.remove(PENDING_KEY);
      setStatus("已关闭：不采集任何 ChatGPT 网页数据");
    }
  } catch (e) {
    setStatus("[!] 操作失败: " + (e && e.message ? e.message : String(e)));
  }
});

refresh();
