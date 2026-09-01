# Donna Desktop

macOS app for Donna’s **local agent runtime**. The existing React app (`donna-web`) is the UI. A Go sidecar (`donna-agent-local`) runs the agent loop on this Mac. Donna cloud remains the source of truth for auth, memory, integrations, run history, and model access.

## Architecture

- **Tauri 2** owns Keychain, OAuth (`donna://auth/callback`), tray, notifications, folder picker, and sidecar supervision.
- **Unix domain socket** (per-launch secret) is the only control channel to the worker. No localhost control port.
- **Go sidecar** claims `execution_target=local` runs, calls `POST /desktop/model/complete`, executes tools locally, and syncs steps to Donna cloud.
- Workspace filesystem paths never leave the Mac. Cloud stores opaque workspace IDs and display names only.

## Develop (from the donna monorepo)

Sibling checkouts required:

```
donna/
  donna-web/
  donna-server-go/
  donna-desktop/   ← this repo
```

```bash
# From the donna monorepo (uses the Railway API, launches Tauri)
npm run dev:desktop

# Local Go API instead:
DONNA_USE_LOCAL_API=1 npm run dev:desktop
```

Ctrl+C stops the desktop app. After sign-in, turn on **Profile → Experimental → Local agents (macOS)**. Production Supabase must have `supabase/migrations/0031_desktop_local_agents.sql` applied.

The desktop window loads `donna-web`. Sign in; the worker registers this Mac and claims local runs.

Add these redirect URLs in Supabase → Authentication → URL Configuration:

- `https://donnadoesit.com/login?desktop_handoff=1`
- `http://localhost:5173/login`
- `http://localhost:5173/login?desktop=1`

Playwright Chromium is required for local browser tools:

```bash
cd browser && npm install && npx playwright install chromium
```

## Build

```bash
npm run sidecar:build
npm run tauri build
```

Signed universal builds and notarization are release-gate items (Apple Developer ID). This repo ships the unsigned local/dev pipeline.

## Security

The app bundle must not contain `SUPABASE_SERVICE_ROLE_KEY`, OpenRouter keys, connector encryption keys, or OAuth client secrets. The worker receives only short-lived access tokens in memory.
