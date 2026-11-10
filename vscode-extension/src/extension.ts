// Kynoptic editor extension (F04, PRD v1.0) — VS Code / Cursor.
//
// Reports real editor events (file focus, language, debug state) to the local
// kynoptic dashboard via POST /api/ext/events (loopback + extension token).
//
// Privacy: this extension NEVER reads file contents. It only sends the file
// path and metadata (language id, debug flag). Nothing leaves the machine:
// the endpoint must be a 127.0.0.1 dashboard.

import * as vscode from "vscode";
import * as http from "http";
import * as https from "https";
import * as crypto from "crypto";

/** One queued editor event (payload shape matches dash POST /api/ext/events). */
interface ExtEvent {
  event_id: string;
  timestamp: string;
  action: string;
  file_path?: string;
  language?: string;
  debug?: boolean;
}

/** Per-flush hard cap; the dashboard rejects batches larger than 200. */
const MAX_BATCH = 200;
/** Debounce window for document_change events (ms), per file. */
const DOC_CHANGE_DEBOUNCE_MS = 10_000;

export function activate(context: vscode.ExtensionContext): void {
  const status = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Right,
    50
  );
  context.subscriptions.push(status);

  const state = new Reporter(status);
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("kynoptic")) {
        state.reloadConfig();
      }
    }),
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      if (editor && editor.document.uri.scheme === "file") {
        state.emitFileEvent("file_open", editor.document);
      }
    }),
    vscode.workspace.onDidChangeTextDocument((e) => {
      if (e.document.uri.scheme === "file") {
        state.emitDocumentChange(e.document);
      }
    }),
    vscode.window.onDidChangeWindowState((ws) => {
      state.emit(ws.focused ? "focus_gained" : "focus_lost");
    }),
    vscode.debug.onDidStartDebugSession(() => {
      state.emitDebugEvent("debug_start", true);
    }),
    vscode.debug.onDidTerminateDebugSession(() => {
      state.emitDebugEvent("debug_end", false);
    }),
    { dispose: () => state.dispose() }
  );
  // Active editor at activation time is a real focus event too.
  const active = vscode.window.activeTextEditor;
  if (active && active.document.uri.scheme === "file") {
    state.emitFileEvent("file_open", active.document);
  }
}

class Reporter {
  private queue: ExtEvent[] = [];
  private timer: NodeJS.Timeout | null = null;
  private inFlight = false;
  /** file path -> last document_change emission (ms), debounce bookkeeping */
  private lastDocChange = new Map<string, number>();
  private endpoint = "http://127.0.0.1:8420";
  private token = "";
  private enabled = true;
  private flushSeconds = 5;
  private readonly status: vscode.StatusBarItem;

  constructor(status: vscode.StatusBarItem) {
    this.status = status;
    this.reloadConfig();
  }

  reloadConfig(): void {
    const cfg = vscode.workspace.getConfiguration("kynoptic");
    this.endpoint = (cfg.get<string>("endpoint") || "http://127.0.0.1:8420").replace(/\/+$/, "");
    this.token = (cfg.get<string>("extToken") || "").trim();
    this.enabled = cfg.get<boolean>("enabled", true);
    this.flushSeconds = Math.max(1, Math.min(60, cfg.get<number>("flushSeconds", 5)));
    this.updateStatus();
    this.ensureTimer();
  }

  dispose(): void {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
    // Best-effort final flush; process teardown may cut it short.
    if (this.queue.length > 0) {
      void this.flush();
    }
  }

  private ensureTimer(): void {
    if (this.timer || !this.enabled) {
      return;
    }
    this.timer = setInterval(() => {
      if (this.queue.length > 0 && !this.inFlight) {
        void this.flush();
      }
    }, this.flushSeconds * 1000);
  }

  private updateStatus(): void {
    if (!this.enabled) {
      this.status.text = "kyn: off";
      this.status.tooltip = "Kynoptic reporting is disabled (kynoptic.enabled).";
    } else if (!this.token) {
      this.status.text = "[!] kyn: no token";
      this.status.tooltip =
        "Set kynoptic.extToken to the token shown on the dashboard settings page (data/ext-token.txt). Events are not reported.";
    } else {
      this.status.text = "kyn: on";
      this.status.tooltip = "Kynoptic is reporting editor events to " + this.endpoint;
    }
    this.status.show();
  }

  /** Source id derived from the host; Cursor identifies itself via uriScheme. */
  private source(): "vscode" | "cursor" {
    return vscode.env.uriScheme === "cursor" ? "cursor" : "vscode";
  }

  emit(action: string): void {
    if (!this.enabled || !this.token) {
      return;
    }
    this.push({ event_id: crypto.randomUUID(), timestamp: new Date().toISOString(), action });
  }

  emitFileEvent(action: string, doc: vscode.TextDocument): void {
    if (!this.enabled || !this.token) {
      return;
    }
    this.push({
      event_id: crypto.randomUUID(),
      timestamp: new Date().toISOString(),
      action,
      file_path: doc.uri.fsPath,
      language: doc.languageId,
    });
  }

  /** Debounced per file: heavy typing must not flood the local endpoint. */
  emitDocumentChange(doc: vscode.TextDocument): void {
    const now = Date.now();
    const last = this.lastDocChange.get(doc.uri.fsPath) || 0;
    if (now - last < DOC_CHANGE_DEBOUNCE_MS) {
      return;
    }
    this.lastDocChange.set(doc.uri.fsPath, now);
    this.emitFileEvent("document_change", doc);
  }

  emitDebugEvent(action: string, debug: boolean): void {
    if (!this.enabled || !this.token) {
      return;
    }
    this.push({
      event_id: crypto.randomUUID(),
      timestamp: new Date().toISOString(),
      action,
      debug,
    });
  }

  private push(ev: ExtEvent): void {
    this.queue.push(ev);
    if (this.queue.length >= MAX_BATCH) {
      void this.flush();
    }
    this.ensureTimer();
  }

  private async flush(): Promise<void> {
    if (this.inFlight || this.queue.length === 0) {
      return;
    }
    this.inFlight = true;
    const batch = this.queue.splice(0, MAX_BATCH);
    const body = JSON.stringify({ source: this.source(), events: batch });
    try {
      await this.post("/api/ext/events", body);
    } catch {
      // Loopback dashboard may be down; put events back at the front so the
      // next successful flush still carries them (bounded: drop oldest if the
      // queue would run unbounded while the dashboard stays down).
      this.queue = batch.concat(this.queue).slice(-MAX_BATCH * 4);
    } finally {
      this.inFlight = false;
    }
  }

  private post(path: string, body: string): Promise<void> {
    return new Promise((resolve, reject) => {
      let url: URL;
      try {
        url = new URL(this.endpoint + path);
      } catch (e) {
        reject(e instanceof Error ? e : new Error(String(e)));
        return;
      }
      const isHttps = url.protocol === "https:";
      const lib = isHttps ? https : http;
      const req = lib.request(
        {
          hostname: url.hostname,
          port: url.port || (isHttps ? 443 : 80),
          path: url.pathname + url.search,
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "Content-Length": Buffer.byteLength(body),
            "X-Kynoptic-Ext-Token": this.token,
          },
          timeout: 5000,
        },
        (res) => {
          // 401 = wrong/revoked token; other non-2xx = server-side rejection.
          // Either way the batch is dropped (retrying auth failures blindly
          // would loop); the status bar reflects configuration problems.
          res.resume();
          if (res.statusCode && res.statusCode >= 200 && res.statusCode < 300) {
            resolve();
          } else {
            reject(new Error("dashboard returned HTTP " + res.statusCode));
          }
        }
      );
      req.on("timeout", () => req.destroy(new Error("dashboard request timed out")));
      req.on("error", reject);
      req.end(body);
    });
  }
}

export function deactivate(): void {
  // Reporter cleanup is registered as a subscription disposable.
}
