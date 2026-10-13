// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::menu::{
    CheckMenuItem, ContextMenu, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::webview::{NewWindowResponse, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{
    AppHandle, Manager, PhysicalPosition, Position, Size, Url, Webview, WebviewUrl,
    WindowEvent, Wry,
};

const MAX_TABS: usize = 3;
const MAIN: &str = "main";

struct Service {
    id: &'static str,
    name: &'static str,
    url: &'static str,
}

static SERVICES: [Service; 4] = [
    Service { id: "chatgpt", name: "ChatGPT", url: "https://chatgpt.com" },
    Service { id: "gemini", name: "Gemini", url: "https://gemini.google.com" },
    Service { id: "deepseek", name: "DeepSeek", url: "https://chat.deepseek.com" },
    Service { id: "claude", name: "Claude", url: "https://claude.ai" },
];

/// Sites that may load inside a tab (the four services plus their sign-in
/// providers). A host matches if it equals an entry or is a subdomain of it.
/// Any other link is opened in your default browser instead, so a tab can't
/// be navigated away from its AI site. Add a domain here if a login flow is
/// being sent to the external browser by mistake.
static ALLOWED_HOSTS: &[&str] = &[
    "chatgpt.com",
    "openai.com",
    "claude.ai",
    "anthropic.com",
    "deepseek.com",
    "google.com",
    "youtube.com", // accounts.youtube.com is part of Google sign-in
    "apple.com",
    "microsoft.com",
    "microsoftonline.com",
    "live.com",
    "cloudflare.com", // bot-check challenges
];

fn service(id: &str) -> Option<&'static Service> {
    SERVICES.iter().find(|s| s.id == id)
}

fn in_app(url: &Url) -> bool {
    match url.scheme() {
        "http" | "https" => url.host_str().is_some_and(|host| {
            ALLOWED_HOSTS.iter().any(|allowed| {
                host == *allowed
                    || host
                        .strip_suffix(*allowed)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            })
        }),
        "about" | "blob" | "data" | "chrome-error" => true,
        _ => false,
    }
}

/// Opens an http(s) link in the default browser (no shell involved).
fn open_external(url: &Url) {
    if !matches!(url.scheme(), "http" | "https") {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url.as_str())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
    }
    #[cfg(not(windows))]
    let _ = url;
}

struct Slot {
    service: &'static Service,
    label: String,    // webview label, unique for the lifetime of the app
    background: bool, // true once WebView2's low-memory target is applied
}

#[derive(Default)]
struct Tabs {
    slots: [Option<Slot>; MAX_TABS],
    active: Option<usize>,
    next_id: u32,
    saved: Option<&'static str>, // last service id written to disk
    // Native menu handles, kept so their text/check state can be refreshed.
    menu: Vec<CheckMenuItem<Wry>>,
    close: Option<MenuItem<Wry>>,
    // "Replace which tab?" popup state.
    pending: Option<&'static Service>,
    popup: Option<Menu<Wry>>,
}

enum Outcome {
    Done,
    Full,
}

// ------------------------------------------------ remember the last service

fn last_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_local_data_dir().ok().map(|d| d.join("last-tab.txt"))
}

/// The service that was active when the app was last used (default: ChatGPT).
/// Only this one tab is reopened; the others are never restored or preloaded.
fn read_last(app: &AppHandle) -> &'static str {
    last_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| service(s.trim()).map(|svc| svc.id))
        .unwrap_or("chatgpt")
}

fn save_last(app: &AppHandle, id: &str) {
    if let Some(path) = last_path(app) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, id);
    }
}

// ------------------------------------------------- webview lifecycle & memory

fn create_tab(app: &AppHandle, label: &str, url: &str) -> Result<(), String> {
    let window = app.get_window(MAIN).ok_or("main window missing")?;
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let url: Url = url.parse().map_err(|e| format!("bad url: {e}"))?;

    let nav_app = app.clone();
    let nav_label = label.to_string();

    let builder = WebviewBuilder::new(label, WebviewUrl::External(url))
        // Keep the tab on its AI site; send everything else to the browser.
        .on_navigation(|url| {
            if in_app(url) {
                true
            } else {
                open_external(url);
                false
            }
        })
        // Links with target=_blank / window.open never create an extra webview
        // (that would break the 3-webview limit). Sign-in links stay in this
        // tab; other links go to the default browser.
        .on_new_window(move |url, _features| {
            if in_app(&url) {
                let app = nav_app.clone();
                let label = nav_label.clone();
                let _ = nav_app.run_on_main_thread(move || {
                    if let Some(wv) = app.get_webview(&label) {
                        let _ = wv.navigate(url);
                    }
                });
            } else {
                open_external(&url);
            }
            NewWindowResponse::Deny
        });

    window
        .add_child(
            builder,
            Position::Physical(PhysicalPosition::new(0, 0)),
            Size::Physical(size),
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Closing a webview destroys its WebView2 controller, which frees that tab's
/// renderer, JS heap and in-memory page state.
fn destroy_tab(app: &AppHandle, label: &str) {
    if let Some(wv) = app.get_webview(label) {
        let _ = wv.close();
    }
}

/// Asks WebView2 to trim memory for a background tab (low = true) or restore
/// normal behaviour (low = false). Scripts keep running, so a reply that is
/// still streaming in a background tab is not interrupted. Best effort: if the
/// WebView2 runtime is older than 1.0.1823.32 the call is silently skipped.
#[cfg(windows)]
fn set_memory_level(wv: &Webview, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    use windows::core::Interface;

    let _ = wv.with_webview(move |platform| unsafe {
        if let Ok(core) = platform.controller().CoreWebView2() {
            if let Ok(core19) = core.cast::<ICoreWebView2_19>() {
                let level = if low {
                    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
                } else {
                    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
                };
                let _ = core19.SetMemoryUsageTargetLevel(level);
            }
        }
    });
}

#[cfg(not(windows))]
fn set_memory_level(_wv: &Webview, _low: bool) {}

/// The active tab fills the window; every other tab is hidden and put on the
/// low-memory target. Nothing is created or destroyed here.
fn apply_layout(app: &AppHandle, tabs: &mut Tabs, focus: bool) {
    let Some(window) = app.get_window(MAIN) else { return };
    let Ok(size) = window.inner_size() else { return };
    if size.width == 0 || size.height == 0 {
        return; // minimized
    }
    let active = tabs.active;
    for (i, slot) in tabs.slots.iter_mut().enumerate() {
        let Some(slot) = slot else { continue };
        let Some(wv) = app.get_webview(&slot.label) else { continue };
        if active == Some(i) {
            if slot.background {
                set_memory_level(&wv, false);
                slot.background = false;
            }
            let _ = wv.set_position(Position::Physical(PhysicalPosition::new(0, 0)));
            let _ = wv.set_size(Size::Physical(size));
            let _ = wv.show();
            if focus {
                let _ = wv.set_focus();
            }
        } else {
            let _ = wv.hide();
            if !slot.background {
                set_memory_level(&wv, true);
                slot.background = true;
            }
        }
    }
}

/// Syncs the native menu (tab names, check mark, enabled state), the window
/// title and the remembered last service with the current state.
fn refresh(app: &AppHandle, tabs: &mut Tabs) {
    for (i, item) in tabs.menu.iter().enumerate() {
        match &tabs.slots[i] {
            Some(s) => {
                let _ = item.set_text(format!("{}  {}", i + 1, s.service.name));
                let _ = item.set_enabled(true);
            }
            None => {
                let _ = item.set_text(format!("{}  (empty)", i + 1));
                let _ = item.set_enabled(false);
            }
        }
        let _ = item.set_checked(tabs.active == Some(i));
    }
    if let Some(close) = &tabs.close {
        let _ = close.set_enabled(tabs.active.is_some());
    }

    let active_service = tabs.active.and_then(|i| tabs.slots[i].as_ref()).map(|s| s.service);

    if let Some(window) = app.get_window(MAIN) {
        let title = match active_service {
            Some(s) => format!("{} - AI Browser", s.name),
            None => "AI Browser".to_string(),
        };
        let _ = window.set_title(&title);
    }

    // Written only when the active service actually changes.
    if let Some(s) = active_service {
        if tabs.saved != Some(s.id) {
            tabs.saved = Some(s.id);
            save_last(app, s.id);
        }
    }
}

// ------------------------------------------------------------- tab actions

/// Opens a service, or switches to it if it is already open. When all slots
/// are used and `replace` is None it returns Outcome::Full so the caller can
/// ask which tab to replace.
fn open_service(
    app: &AppHandle,
    tabs: &mut Tabs,
    svc: &'static Service,
    replace: Option<usize>,
) -> Result<Outcome, String> {
    let already = tabs
        .slots
        .iter()
        .position(|s| s.as_ref().is_some_and(|s| s.service.id == svc.id));

    if let Some(i) = already {
        tabs.active = Some(i); // switch only, no new webview
    } else {
        let slot = match tabs.slots.iter().position(Option::is_none) {
            Some(free) => free,
            None => match replace {
                Some(i) if i < MAX_TABS => {
                    // Destroy the old webview first so we never hold a 4th.
                    if let Some(old) = tabs.slots[i].take() {
                        destroy_tab(app, &old.label);
                    }
                    i
                }
                _ => return Ok(Outcome::Full),
            },
        };

        let label = format!("tab-{}", tabs.next_id);
        tabs.next_id += 1;

        if let Err(e) = create_tab(app, &label, svc.url) {
            let next = tabs.slots.iter().position(Option::is_some);
            tabs.active = next;
            apply_layout(app, tabs, false);
            refresh(app, tabs);
            return Err(e);
        }
        tabs.slots[slot] = Some(Slot { service: svc, label, background: false });
        tabs.active = Some(slot);
    }

    apply_layout(app, tabs, true);
    refresh(app, tabs);
    Ok(Outcome::Done)
}

fn close_slot(app: &AppHandle, tabs: &mut Tabs, slot: usize) {
    if let Some(old) = tabs.slots.get_mut(slot).and_then(Option::take) {
        destroy_tab(app, &old.label);
    }
    if tabs.active == Some(slot) {
        let next = tabs.slots.iter().position(Option::is_some);
        tabs.active = next;
    }
    apply_layout(app, tabs, true);
    refresh(app, tabs);
}

/// Native popup shown next to the cursor when all 3 slots are in use.
fn build_replace_menu(
    app: &AppHandle,
    tabs: &Tabs,
    svc: &Service,
) -> tauri::Result<Menu<Wry>> {
    let header = MenuItem::with_id(
        app,
        "noop",
        format!("All 3 tabs in use. Replace which with {}?", svc.name),
        false,
        None::<&str>,
    )?;
    let sep = PredefinedMenuItem::separator(app)?;
    let mut items = Vec::new();
    for (i, slot) in tabs.slots.iter().enumerate() {
        if let Some(slot) = slot {
            items.push(MenuItem::with_id(
                app,
                format!("replace-{i}"),
                format!("Replace {}", slot.service.name),
                true,
                None::<&str>,
            )?);
        }
    }
    let mut refs: Vec<&dyn IsMenuItem<Wry>> = vec![&header, &sep];
    refs.extend(items.iter().map(|i| i as &dyn IsMenuItem<Wry>));
    Menu::with_items(app, &refs)
}

fn handle_menu(app: &AppHandle, id: &str) {
    let state = app.state::<Mutex<Tabs>>();
    let mut ask: Option<Menu<Wry>> = None;

    {
        let Ok(mut tabs) = state.lock() else { return };

        if id == "close" {
            if let Some(i) = tabs.active {
                close_slot(app, &mut tabs, i);
            }
        } else if let Some(n) = id.strip_prefix("slot-").and_then(|n| n.parse::<usize>().ok()) {
            if n < MAX_TABS && tabs.slots[n].is_some() {
                tabs.active = Some(n);
                apply_layout(app, &mut tabs, true);
                refresh(app, &mut tabs);
            }
        } else if let Some(svc) = id.strip_prefix("new-").and_then(service) {
            if let Ok(Outcome::Full) = open_service(app, &mut tabs, svc, None) {
                tabs.pending = Some(svc);
                ask = build_replace_menu(app, &tabs, svc).ok();
                tabs.popup = ask.clone(); // keep it alive until the choice arrives
            }
        } else if let Some(n) = id.strip_prefix("replace-").and_then(|n| n.parse::<usize>().ok()) {
            if let Some(svc) = tabs.pending.take() {
                let _ = open_service(app, &mut tabs, svc, Some(n));
            }
            tabs.popup = None;
        }
    } // lock released before the (blocking) popup is shown

    if let (Some(menu), Some(window)) = (ask, app.get_window(MAIN)) {
        let _ = menu.popup(window);
    }
}

// -------------------------------------------------------------------- main

fn main() {
    tauri::Builder::default()
        .manage(Mutex::new(Tabs::default()))
        .on_menu_event(|app, event| {
            let id: &str = event.id().as_ref();
            handle_menu(app, id);
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Resized(_) = event {
                if window.label() == MAIN {
                    let app = window.app_handle();
                    let state = app.state::<Mutex<Tabs>>();
                    // try_lock: this can fire while a menu action holds the lock
                    // (same thread); that action lays out the webviews itself.
                    if let Ok(mut tabs) = state.try_lock() {
                        apply_layout(app, &mut tabs, false);
                    };
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();

            let window = WindowBuilder::new(&handle, MAIN)
                .title("AI Browser")
                .inner_size(1200.0, 800.0)
                .min_inner_size(600.0, 400.0)
                .build()?;

            // Native menu bar replaces a tab-bar webview (no extra process).
            let slot_items = (0..MAX_TABS)
                .map(|i| {
                    CheckMenuItem::with_id(
                        &handle,
                        format!("slot-{i}"),
                        format!("{}  (empty)", i + 1),
                        false,
                        false,
                        Some(format!("CmdOrCtrl+{}", i + 1)),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            let close = MenuItem::with_id(&handle, "close", "Close Tab", false, Some("CmdOrCtrl+W"))?;
            let sep = PredefinedMenuItem::separator(&handle)?;
            let mut tab_refs: Vec<&dyn IsMenuItem<Wry>> =
                slot_items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
            tab_refs.push(&sep);
            tab_refs.push(&close);
            let tabs_menu = Submenu::with_items(&handle, "Tabs", true, &tab_refs)?;

            let new_items = SERVICES
                .iter()
                .map(|s| MenuItem::with_id(&handle, format!("new-{}", s.id), s.name, true, None::<&str>))
                .collect::<Result<Vec<_>, _>>()?;
            let new_refs: Vec<&dyn IsMenuItem<Wry>> =
                new_items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
            let new_menu = Submenu::with_items(&handle, "New Tab", true, &new_refs)?;

            window.set_menu(Menu::with_items(&handle, &[&tabs_menu, &new_menu])?)?;

            {
                let state = handle.state::<Mutex<Tabs>>();
                if let Ok(mut tabs) = state.lock() {
                    tabs.menu = slot_items;
                    tabs.close = Some(close);
                };
            }

            // Exactly one tab at startup (the one used last time); nothing else
            // is created or preloaded until you ask for it.
            let start = read_last(&handle);
            handle_menu(&handle, &format!("new-{start}"));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running AI Browser");
}