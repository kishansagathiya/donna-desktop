import http from "node:http";
import { chromium } from "playwright";

// Prefer platform PORT (Railway/Heroku); DONNA_BROWSER_PORT for local overrides.
const PORT = Number(process.env.PORT || process.env.DONNA_BROWSER_PORT || 9229);
const HOST = process.env.DONNA_BROWSER_HOST || (process.env.PORT ? "0.0.0.0" : "127.0.0.1");
const MAX_CONCURRENCY = Number(process.env.DONNA_BROWSER_CONCURRENCY || 2);
const MAX_SESSIONS = Number(process.env.DONNA_BROWSER_MAX_SESSIONS || 8);
const SESSION_TTL_MS = Number(process.env.DONNA_BROWSER_SESSION_TTL_MS || 15 * 60 * 1000);
const NAV_TIMEOUT_MS = Number(process.env.DONNA_BROWSER_NAV_TIMEOUT_MS || 20_000);
const SESSION_ID_RE = /^[a-zA-Z0-9_-]{8,64}$/;

const USER_DATA_DIR = process.env.DONNA_BROWSER_USER_DATA_DIR || "";
const HEADED = process.env.DONNA_BROWSER_HEADED === "1";

let browserPromise = null;
let active = 0;
const sessions = new Map();
const pendingSessions = new Map();

async function getBrowser() {
  if (!browserPromise) {
    const args = ["--disable-dev-shm-usage", "--no-sandbox"];
    if (USER_DATA_DIR) {
      args.push(`--user-data-dir=${USER_DATA_DIR}`);
    }
    browserPromise = chromium.launch({
      headless: !HEADED,
      args,
    });
  }
  return browserPromise;
}

function clampText(text, maxChars) {
  const trimmed = String(text || "").trim();
  if (!maxChars || trimmed.length <= maxChars) return trimmed;
  return `${trimmed.slice(0, maxChars)}\n\n[truncated]`;
}

function httpError(status, message) {
  const err = new Error(message);
  err.status = status;
  return err;
}

async function readJSON(req, limit = 100_000) {
  let raw = "";
  for await (const chunk of req) {
    raw += chunk;
    if (raw.length > limit) {
      throw httpError(413, "request too large");
    }
  }
  try {
    return JSON.parse(raw || "{}");
  } catch {
    throw httpError(400, "invalid json");
  }
}

async function extractMain(page, maxChars = 16_000) {
  const extracted = await page.evaluate(() => {
    const article =
      document.querySelector("article") ||
      document.querySelector("main") ||
      document.body;
    const text = (article?.innerText || "").replace(/\n{3,}/g, "\n\n").trim();
    return {
      title: document.title || "",
      text,
    };
  });
  return {
    url: page.url(),
    title: extracted.title,
    text: clampText(extracted.text, maxChars),
  };
}

async function snapshotPage(page) {
  const extracted = await page.evaluate(() => {
    document.querySelectorAll("[data-donna-ref]").forEach((el) => {
      el.removeAttribute("data-donna-ref");
    });
    const SELECT =
      'a, button, input, textarea, select, [role="button"], [role="link"], [contenteditable="true"]';
    const nodes = Array.from(document.querySelectorAll(SELECT));
    const elements = [];
    let i = 0;
    for (const el of nodes) {
      if (i >= 80) break;
      const style = window.getComputedStyle(el);
      if (style.display === "none" || style.visibility === "hidden") continue;
      const rect = el.getBoundingClientRect();
      if (rect.width === 0 && rect.height === 0) continue;
      i += 1;
      const ref = `e${i}`;
      el.setAttribute("data-donna-ref", ref);
      const name = (
        el.getAttribute("aria-label") ||
        el.getAttribute("name") ||
        el.getAttribute("placeholder") ||
        el.getAttribute("title") ||
        (el.innerText || "").trim()
      ).slice(0, 80);
      elements.push({
        ref,
        tag: (el.tagName || "").toLowerCase(),
        type: el.getAttribute("type") || "",
        name,
        role: el.getAttribute("role") || "",
      });
    }
    const article =
      document.querySelector("article") ||
      document.querySelector("main") ||
      document.body;
    const text = (article?.innerText || "").replace(/\n{3,}/g, "\n\n").trim();
    return {
      title: document.title || "",
      text,
      elements,
    };
  });
  return {
    url: page.url(),
    title: extracted.title,
    text: clampText(extracted.text, 8_000),
    elements: extracted.elements || [],
  };
}

async function browsePage({ url, wait_ms: waitMs = 0, max_chars: maxChars = 16_000 }) {
  if (active >= MAX_CONCURRENCY) {
    throw httpError(429, "browser concurrency limit reached");
  }
  active += 1;
  const browser = await getBrowser();
  const context = await browser.newContext({
    userAgent: "DonnaBrowser/1.0 (+https://github.com/kishansagathiya/donna)",
    javaScriptEnabled: true,
  });
  const page = await context.newPage();
  try {
    const response = await page.goto(url, {
      waitUntil: "domcontentloaded",
      timeout: NAV_TIMEOUT_MS,
    });
    const pauseMs = waitMs > 0 ? Math.min(waitMs, 10_000) : 500;
    await new Promise((resolve) => setTimeout(resolve, pauseMs));
    const extracted = await extractMain(page, maxChars);
    return {
      ...extracted,
      status: response?.status() ?? 0,
    };
  } finally {
    await context.close().catch(() => {});
    active -= 1;
  }
}

async function closeSessionRecord(sess) {
  if (!sess) return;
  sessions.delete(sess.id);
  await sess.context.close().catch(() => {});
}

async function getOrCreateSession(id) {
  const existing = sessions.get(id);
  if (existing) {
    existing.lastUsed = Date.now();
    return existing;
  }
  if (pendingSessions.has(id)) {
    return pendingSessions.get(id);
  }
  const pending = (async () => {
    if (sessions.size >= MAX_SESSIONS) {
      throw httpError(429, "browser session limit reached");
    }
    const browser = await getBrowser();
    const context = await browser.newContext({
      userAgent: "DonnaBrowser/1.0 (+https://github.com/kishansagathiya/donna)",
      javaScriptEnabled: true,
    });
    const page = await context.newPage();
    const sess = { id, context, page, lastUsed: Date.now() };
    sessions.set(id, sess);
    return sess;
  })();
  pendingSessions.set(id, pending);
  try {
    return await pending;
  } catch (err) {
    throw err;
  } finally {
    pendingSessions.delete(id);
  }
}

async function requireSession(id) {
  const sess = sessions.get(id);
  if (!sess) {
    throw httpError(404, "session not found");
  }
  sess.lastUsed = Date.now();
  return sess;
}

function requireSessionID(body) {
  const id = String(body?.session_id || "").trim();
  if (!SESSION_ID_RE.test(id)) {
    throw httpError(400, "session_id is required");
  }
  return id;
}

async function sweepSessions() {
  const now = Date.now();
  for (const sess of [...sessions.values()]) {
    if (now - sess.lastUsed > SESSION_TTL_MS) {
      await closeSessionRecord(sess);
    }
  }
}

setInterval(() => {
  sweepSessions().catch(() => {});
}, 60_000).unref?.();

function sendJSON(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(payload),
  });
  res.end(payload);
}

async function handleSession(req, res, action) {
  const body = await readJSON(req);
  const sessionID = requireSessionID(body);
  if (action === "close") {
    const sess = sessions.get(sessionID);
    await closeSessionRecord(sess);
    sendJSON(res, 200, { ok: true, session_id: sessionID });
    return;
  }

  let sess;
  if (action === "navigate") {
    sess = await getOrCreateSession(sessionID);
  } else {
    sess = await requireSession(sessionID);
  }

  if (action === "navigate") {
    const url = String(body.url || "").trim();
    if (!url) {
      throw httpError(400, "url is required");
    }
    const response = await sess.page.goto(url, {
      waitUntil: "domcontentloaded",
      timeout: NAV_TIMEOUT_MS,
    });
    const pauseMs = body.wait_ms > 0 ? Math.min(body.wait_ms, 10_000) : 300;
    await new Promise((resolve) => setTimeout(resolve, pauseMs));
    const snap = await snapshotPage(sess.page);
    sendJSON(res, 200, { ...snap, status: response?.status() ?? 0 });
    return;
  }

  if (action === "snapshot") {
    const snap = await snapshotPage(sess.page);
    sendJSON(res, 200, snap);
    return;
  }

  if (action === "extract") {
    const maxChars = Number(body.max_chars) > 0 ? Number(body.max_chars) : 16_000;
    const extracted = await extractMain(sess.page, maxChars);
    sendJSON(res, 200, extracted);
    return;
  }

  const ref = String(body.ref || "").trim();
  if (!/^e\d+$/.test(ref)) {
    throw httpError(400, "ref is required");
  }
  const locator = sess.page.locator(`[data-donna-ref="${ref}"]`).first();
  const count = await locator.count();
  if (count === 0) {
    throw httpError(404, `element ${ref} not found; call browser_snapshot again`);
  }

  if (action === "click") {
    await locator.click({ timeout: 8_000 });
    await new Promise((resolve) => setTimeout(resolve, 250));
    const snap = await snapshotPage(sess.page);
    sendJSON(res, 200, snap);
    return;
  }

  if (action === "type") {
    const text = String(body.text ?? "");
    await locator.fill(text, { timeout: 8_000 });
    if (body.submit) {
      await locator.press("Enter").catch(() => {});
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    const snap = await snapshotPage(sess.page);
    sendJSON(res, 200, snap);
    return;
  }

  throw httpError(404, "not found");
}

const SESSION_ROUTES = {
  "/session/navigate": "navigate",
  "/session/snapshot": "snapshot",
  "/session/click": "click",
  "/session/type": "type",
  "/session/extract": "extract",
  "/session/close": "close",
};

const server = http.createServer(async (req, res) => {
  try {
    if (req.method === "GET" && req.url === "/health") {
      sendJSON(res, 200, {
        ok: true,
        service: "donna-browser",
        active,
        sessions: sessions.size,
      });
      return;
    }

    if (req.method === "POST" && req.url === "/browse") {
      const body = await readJSON(req);
      if (!body.url || typeof body.url !== "string") {
        sendJSON(res, 400, { error: "url is required" });
        return;
      }
      const result = await browsePage(body);
      sendJSON(res, 200, result);
      return;
    }

    if (req.method === "POST" && SESSION_ROUTES[req.url]) {
      await handleSession(req, res, SESSION_ROUTES[req.url]);
      return;
    }

    sendJSON(res, 404, { error: "not found" });
  } catch (err) {
    const status = err?.status || 500;
    sendJSON(res, status, { error: err?.message || "browse failed" });
  }
});

server.listen(PORT, HOST, () => {
  const addr = server.address();
  const port = typeof addr === "object" && addr ? addr.port : PORT;
  const url = `http://${HOST}:${port}`;
  console.log(`LISTEN ${url}`);
  console.log(`[donna-browser] listening on ${url}`);
});

async function shutdown() {
  try {
    for (const sess of [...sessions.values()]) {
      await closeSessionRecord(sess);
    }
    if (browserPromise) {
      const browser = await browserPromise;
      await browser.close();
    }
  } finally {
    process.exit(0);
  }
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
