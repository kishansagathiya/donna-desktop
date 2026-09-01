use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use keyring::Entry;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use uuid::Uuid;

const KEYCHAIN_SERVICE: &str = "com.kishansagathiya.donna.desktop";
const KEYCHAIN_ACCOUNT: &str = "refresh_token";
const DEFAULT_API: &str = "https://donna-server-go-production.up.railway.app";
const DEFAULT_WEB: &str = "https://donnadoesit.com";
const SUPABASE_URL: &str = "https://eghhxjlhautsikejocze.supabase.co";
const SUPABASE_ANON_KEY: &str = "sb_publishable_sFpDOcCxs9aKq283JIQPBg_eZRIpUTB";
const ACCESS_REFRESH_AFTER: Duration = Duration::from_secs(50 * 60);

#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct LocalWorkspace {
    id: String,
    name: String,
    path: String,
}

#[derive(Clone, Serialize)]
struct DesktopStatus {
    app_version: String,
    worker_version: String,
    device_id: String,
    public_device_id: String,
    cloud_connected: bool,
    worker_running: bool,
    paused: bool,
    browser_ready: bool,
    active_run_id: String,
    queued_runs: i64,
}

struct AppState {
    tokens: Mutex<Option<Tokens>>,
    access_issued_at: Mutex<Option<Instant>>,
    public_device_id: String,
    support_dir: PathBuf,
    ipc_socket: PathBuf,
    ipc_secret: String,
    worker: Mutex<Option<Child>>,
    paused: Mutex<bool>,
    workspaces: Mutex<Vec<LocalWorkspace>>,
    api_base: String,
    web_base: String,
}

impl AppState {
    fn keychain() -> Result<Entry, String> {
        Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).map_err(|e| e.to_string())
    }

    fn save_refresh(&self, refresh: &str) -> Result<(), String> {
        Self::keychain()?
            .set_password(refresh)
            .map_err(|e| e.to_string())
    }

    fn load_refresh() -> Option<String> {
        Self::keychain().ok()?.get_password().ok().filter(|s| !s.is_empty())
    }

    fn clear_refresh(&self) {
        if let Ok(entry) = Self::keychain() {
            let _ = entry.delete_credential();
        }
    }

    fn workspaces_path(&self) -> PathBuf {
        self.support_dir.join("workspaces.json")
    }

    fn persist_workspaces(&self) {
        if let Ok(raw) = serde_json::to_vec_pretty(&*self.workspaces.lock().unwrap()) {
            let _ = fs::write(self.workspaces_path(), raw);
        }
    }

    fn load_workspaces(&self) {
        if let Ok(raw) = fs::read(self.workspaces_path()) {
            if let Ok(rows) = serde_json::from_slice::<Vec<LocalWorkspace>>(&raw) {
                *self.workspaces.lock().unwrap() = rows;
            }
        }
    }
}

#[tauri::command]
fn auth_session(state: State<AppState>) -> Option<Tokens> {
    let _ = ensure_fresh_access(&state);
    state
        .tokens
        .lock()
        .unwrap()
        .clone()
        .filter(|t| !t.access_token.is_empty())
}

#[tauri::command]
fn get_access_token(state: State<AppState>) -> Option<String> {
    let _ = ensure_fresh_access(&state);
    state
        .tokens
        .lock()
        .unwrap()
        .as_ref()
        .map(|t| t.access_token.clone())
        .filter(|s| !s.is_empty())
}

#[tauri::command]
fn set_session(
    state: State<AppState>,
    access_token: String,
    refresh_token: String,
) -> Result<(), String> {
    if access_token.is_empty() {
        return Err("missing_access_token".into());
    }
    if !refresh_token.is_empty() {
        state.save_refresh(&refresh_token)?;
    }
    *state.tokens.lock().unwrap() = Some(Tokens {
        access_token: access_token.clone(),
    });
    *state.access_issued_at.lock().unwrap() = Some(Instant::now());
    spawn_worker(&state)?;
    schedule_workspace_sync(&state);
    Ok(())
}

#[tauri::command]
fn auth_start(app: AppHandle, state: State<AppState>, provider: String) -> Result<(), String> {
    let redirect = "donna://auth/callback";
    let url = format!(
        "{}/login?desktop=1&provider={}&redirect_to={}",
        state.web_base,
        urlencoding_lite(&provider),
        urlencoding_lite(redirect)
    );
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn auth_sign_out(state: State<AppState>) -> Result<(), String> {
    *state.tokens.lock().unwrap() = None;
    *state.access_issued_at.lock().unwrap() = None;
    state.clear_refresh();
    stop_worker(&state);
    Ok(())
}

#[tauri::command]
fn pick_workspace(app: AppHandle, state: State<AppState>) -> Result<LocalWorkspace, String> {
    let folder = app
        .dialog()
        .file()
        .blocking_pick_folder()
        .ok_or_else(|| "cancelled".to_string())?;
    let path = folder_path(folder)?;
    let name = Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Workspace")
        .to_string();
    let ws = LocalWorkspace {
        id: Uuid::new_v4().to_string(),
        name,
        path,
    };
    {
        let mut list = state.workspaces.lock().unwrap();
        list.push(ws.clone());
    }
    state.persist_workspaces();
    schedule_workspace_sync(&state);
    Ok(ws)
}

#[tauri::command]
fn list_workspaces(state: State<AppState>) -> Vec<LocalWorkspace> {
    state.workspaces.lock().unwrap().clone()
}

#[tauri::command]
fn diagnostics(state: State<AppState>) -> DesktopStatus {
    let running = worker_alive(&state);
    let mut status = DesktopStatus {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        worker_version: "0.1.0".into(),
        device_id: String::new(),
        public_device_id: state.public_device_id.clone(),
        cloud_connected: state
            .tokens
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|t| !t.access_token.is_empty()),
        worker_running: running,
        paused: *state.paused.lock().unwrap(),
        browser_ready: false,
        active_run_id: String::new(),
        queued_runs: 0,
    };
    if let Some(resp) = ipc_roundtrip(&state, serde_json::json!({"type": "status"})) {
        if let Some(payload) = resp.get("payload") {
            if let Some(v) = payload.get("worker").and_then(|v| v.as_str()) {
                status.worker_version = v.to_string();
            }
            if let Some(v) = payload.get("device_id").and_then(|v| v.as_str()) {
                status.device_id = v.to_string();
            }
            if let Some(v) = payload.get("paused").and_then(|v| v.as_bool()) {
                status.paused = v;
            }
            if let Some(v) = payload.get("active_run_id").and_then(|v| v.as_str()) {
                status.active_run_id = v.to_string();
            }
        }
    }
    status
}

#[tauri::command]
fn restart_worker(state: State<AppState>) -> Result<(), String> {
    let _ = ensure_fresh_access(&state);
    spawn_worker(&state)?;
    schedule_workspace_sync(&state);
    Ok(())
}

#[tauri::command]
fn show_browser(_state: State<AppState>) -> Result<(), String> {
    Ok(())
}

fn folder_path(folder: tauri_plugin_dialog::FilePath) -> Result<String, String> {
    folder
        .into_path()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| e.to_string())
}

fn urlencoding_lite(raw: &str) -> String {
    url::form_urlencoded::byte_serialize(raw.as_bytes()).collect()
}

fn support_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Donna");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn public_device_id(dir: &Path) -> String {
    let path = dir.join("device-id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let id = Uuid::new_v4().to_string();
    let _ = fs::write(path, &id);
    id
}

fn random_secret() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn stop_worker(state: &AppState) {
    if let Some(mut child) = state.worker.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn worker_alive(state: &AppState) -> bool {
    state
        .worker
        .lock()
        .unwrap()
        .as_mut()
        .map(|c| c.try_wait().ok().flatten().is_none())
        .unwrap_or(false)
}

fn sidecar_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let dir = exe.parent().unwrap_or(Path::new("."));
    let bundled = dir.join("donna-agent-local");
    if bundled.exists() {
        return bundled;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(format!("donna-agent-local-{}-apple-darwin", current_triple_arch()))
}

fn current_triple_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        _ => "aarch64",
    }
}

fn server_go_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../donna-server-go")
}

fn spawn_worker(state: &AppState) -> Result<(), String> {
    stop_worker(state);
    let tokens = state.tokens.lock().unwrap().clone();
    let Some(tokens) = tokens else {
        return Ok(());
    };
    if tokens.access_token.is_empty() {
        return Ok(());
    }
    let bundled = sidecar_bin();
    let mut cmd = if bundled.exists() {
        Command::new(bundled)
    } else {
        let mut c = Command::new("go");
        c.current_dir(server_go_dir());
        c.args(["run", "./cmd/donna-agent-local"]);
        c
    };
    let browser_script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../browser/server.js");
    let _ = fs::remove_file(&state.ipc_socket);
    cmd.env("DONNA_API_BASE", &state.api_base)
        .env("DONNA_ACCESS_TOKEN", &tokens.access_token)
        .env("DONNA_IPC_SOCKET", &state.ipc_socket)
        .env("DONNA_IPC_SECRET", &state.ipc_secret)
        .env("DONNA_SUPPORT_DIR", &state.support_dir)
        .env("DONNA_PUBLIC_DEVICE_ID", &state.public_device_id)
        .env("DONNA_DEVICE_NAME", hostname())
        .env("DONNA_DEVICE_ARCH", std::env::consts::ARCH)
        .env("DONNA_BROWSER_SCRIPT", browser_script)
        .env("DONNA_BROWSER_HEADED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    *state.worker.lock().unwrap() = Some(child);
    Ok(())
}

fn hostname() -> String {
    Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

fn schedule_workspace_sync(state: &AppState) {
    let socket = state.ipc_socket.clone();
    let secret = state.ipc_secret.clone();
    let workspaces = state.workspaces.lock().unwrap().clone();
    thread::spawn(move || {
        let payload = serde_json::json!({
            "type": "workspaces",
            "payload": { "workspaces": workspaces }
        });
        for _ in 0..50 {
            if ipc_send(&socket, &secret, payload.clone()).is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

fn ipc_send(socket: &Path, secret: &str, msg: serde_json::Value) -> Option<serde_json::Value> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    let auth = serde_json::json!({"type":"auth","payload":{"secret": secret}});
    writeln!(stream, "{auth}").ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    writeln!(stream, "{msg}").ok()?;
    line.clear();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn ipc_roundtrip(state: &AppState, msg: serde_json::Value) -> Option<serde_json::Value> {
    ipc_send(&state.ipc_socket, &state.ipc_secret, msg)
}

fn send_pause(state: &AppState, paused: bool) {
    let kind = if paused { "pause" } else { "resume" };
    let _ = ipc_roundtrip(state, serde_json::json!({"type": kind}));
}

fn ensure_fresh_access(state: &AppState) -> Result<(), String> {
    let has_fresh = {
        let tokens = state.tokens.lock().unwrap();
        let issued = state.access_issued_at.lock().unwrap();
        tokens
            .as_ref()
            .is_some_and(|t| !t.access_token.is_empty())
            && issued.is_some_and(|at| at.elapsed() < ACCESS_REFRESH_AFTER)
    };
    if has_fresh {
        return Ok(());
    }
    refresh_access(state)
}

fn refresh_access(state: &AppState) -> Result<(), String> {
    let Some(refresh) = AppState::load_refresh() else {
        return Ok(());
    };
    let supabase_url = std::env::var("DONNA_SUPABASE_URL").unwrap_or_else(|_| SUPABASE_URL.into());
    let anon = std::env::var("DONNA_SUPABASE_ANON_KEY").unwrap_or_else(|_| SUPABASE_ANON_KEY.into());
    let url = format!("{supabase_url}/auth/v1/token?grant_type=refresh_token");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(url)
        .header("apikey", &anon)
        .header("Authorization", format!("Bearer {anon}"))
        .json(&serde_json::json!({ "refresh_token": refresh }))
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("token_refresh_{}", resp.status()));
    }
    let body: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    let access = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if access.is_empty() {
        return Err("token_refresh_empty".into());
    }
    if let Some(next_refresh) = body.get("refresh_token").and_then(|v| v.as_str()) {
        if !next_refresh.is_empty() {
            state.save_refresh(next_refresh)?;
        }
    }
    *state.tokens.lock().unwrap() = Some(Tokens {
        access_token: access.clone(),
    });
    *state.access_issued_at.lock().unwrap() = Some(Instant::now());
    let _ = ipc_roundtrip(
        state,
        serde_json::json!({"type":"token","payload":{"access_token": access}}),
    );
    Ok(())
}

fn handle_deep_link(app: &AppHandle, url: &str) {
    let Ok(parsed) = url::Url::parse(url) else {
        return;
    };
    if parsed.scheme() != "donna" {
        return;
    }
    let mut access = String::new();
    let mut refresh = String::new();
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "access_token" => access = v.into_owned(),
            "refresh_token" => refresh = v.into_owned(),
            _ => {}
        }
    }
    if let Some(frag) = parsed.fragment() {
        for pair in frag.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                let decoded = url::form_urlencoded::parse(v.as_bytes())
                    .next()
                    .map(|(_, val)| val.into_owned())
                    .unwrap_or_else(|| v.to_string());
                match k {
                    "access_token" => access = decoded,
                    "refresh_token" => refresh = decoded,
                    _ => {}
                }
            }
        }
    }
    if access.is_empty() {
        return;
    }
    if let Some(state) = app.try_state::<AppState>() {
        if !refresh.is_empty() {
            let _ = state.save_refresh(&refresh);
        }
        *state.tokens.lock().unwrap() = Some(Tokens {
            access_token: access,
        });
        *state.access_issued_at.lock().unwrap() = Some(Instant::now());
        let _ = spawn_worker(&state);
        schedule_workspace_sync(&state);
        let _ = app.emit("donna://auth", serde_json::json!({"ok": true}));
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.show();
            let _ = win.set_focus();
        }
        let _ = app
            .notification()
            .builder()
            .title("Donna")
            .body("Signed in. Local agent worker is starting.")
            .show();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Donna", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause new runs", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "Restart worker", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Donna", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &pause, &restart, &quit])?;
    TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            "pause" => {
                if let Some(state) = app.try_state::<AppState>() {
                    let mut paused = state.paused.lock().unwrap();
                    *paused = !*paused;
                    send_pause(&state, *paused);
                }
            }
            "restart" => {
                if let Some(state) = app.try_state::<AppState>() {
                    let _ = ensure_fresh_access(&state);
                    let _ = spawn_worker(&state);
                    schedule_workspace_sync(&state);
                }
            }
            "quit" => {
                if let Some(state) = app.try_state::<AppState>() {
                    stop_worker(&state);
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                if let Some(win) = tray.app_handle().get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let support = support_dir();
    let public_id = public_device_id(&support);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let ipc_socket = std::env::temp_dir().join(format!("donna-desktop-{nonce}.sock"));
    let api_base = std::env::var("DONNA_API_BASE").unwrap_or_else(|_| DEFAULT_API.into());
    let web_base = std::env::var("DONNA_WEB_APP_BASE").unwrap_or_else(|_| DEFAULT_WEB.into());
    let state = AppState {
        tokens: Mutex::new(None),
        access_issued_at: Mutex::new(None),
        public_device_id: public_id,
        support_dir: support,
        ipc_socket,
        ipc_secret: random_secret(),
        worker: Mutex::new(None),
        paused: Mutex::new(false),
        workspaces: Mutex::new(Vec::new()),
        api_base,
        web_base,
    };
    state.load_workspaces();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_deep_link::init())
        .manage(state)
        .setup(|app| {
            build_tray(app.handle())?;
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    handle_deep_link(&handle, url.as_str());
                }
            });
            if let Some(state) = app.try_state::<AppState>() {
                if AppState::load_refresh().is_some() {
                    if ensure_fresh_access(&state).is_ok() {
                        let _ = spawn_worker(&state);
                        schedule_workspace_sync(&state);
                    }
                }
            }
            if let Some(win) = app.get_webview_window("main") {
                let handle = app.handle().clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Some(w) = handle.get_webview_window("main") {
                            let _ = w.hide();
                        }
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            auth_session,
            get_access_token,
            set_session,
            auth_start,
            auth_sign_out,
            pick_workspace,
            list_workspaces,
            diagnostics,
            restart_worker,
            show_browser
        ])
        .run(tauri::generate_context!())
        .expect("error while running Donna Desktop");
}
