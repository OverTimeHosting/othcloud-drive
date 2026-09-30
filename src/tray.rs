//! Tray icon + menu. The event loop (tao) owns the UI thread; anything that
//! touches the network runs on a worker thread and posts `Refresh` back.

use crate::app::App;
use crate::platform;
use parking_lot::Mutex;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug)]
pub enum UserEvent {
    Menu(MenuEvent),
    Refresh,
    Link(String),
    Error(String),
    Quit,
}

type Shared = Arc<Mutex<App>>;

fn icon(state: u8) -> Icon {
    let (rgba, w, h) = crate::icons::tray_rgba(state);
    Icon::from_rgba(rgba, w, h).expect("tray icon")
}

/// Run `f` against the app on a worker thread, then refresh the menu.
fn work<F: FnOnce(&mut App) -> Result<(), String> + Send + 'static>(
    app: &Shared,
    proxy: &EventLoopProxy<UserEvent>,
    f: F,
) {
    let (app, proxy) = (app.clone(), proxy.clone());
    std::thread::spawn(move || {
        let r = {
            let mut a = app.lock();
            let r = f(&mut a);
            if let Err(e) = &r {
                a.last_error = Some(e.clone());
            }
            r
        };
        if let Err(e) = r {
            let _ = proxy.send_event(UserEvent::Error(e));
        }
        let _ = proxy.send_event(UserEvent::Refresh);
    });
}

fn build_menu(a: &App) -> Menu {
    let menu = Menu::new();
    let add = |m: &Menu, item: &dyn tray_icon::menu::IsMenuItem| {
        let _ = m.append(item);
    };
    let who = a
        .creds
        .as_ref()
        .map(|c| {
            c.user_email
                .clone()
                .or(c.user_name.clone())
                .unwrap_or_else(|| c.origin.clone())
        })
        .unwrap_or_default();
    add(&menu, &MenuItem::with_id("status", format!("OTHCloud Drive — {}", a.status_line()), false, None));
    if !who.is_empty() {
        add(&menu, &MenuItem::with_id("who", format!("Signed in as {who}"), false, None));
    }
    if let Some(e) = &a.last_error {
        let short: String = e.chars().take(90).collect();
        add(&menu, &MenuItem::with_id("err", format!("⚠ {short}"), false, None));
    }
    add(&menu, &PredefinedMenuItem::separator());

    if a.signed_in() && a.api.is_some() {
        if a.drives.is_empty() {
            add(&menu, &MenuItem::with_id("nodrives", "No drives in this organization", false, None));
        }
        for d in &a.drives {
            let mounted = a.mounts.get(&d.storage_id);
            let title = match mounted {
                Some(m) => format!("{} ({}:)", d.name, m.letter),
                None => d.name.clone(),
            };
            let sub = Submenu::new(title, true);
            let _ = sub.append(&CheckMenuItem::with_id(
                format!("mount:{}", d.storage_id),
                "Mounted",
                a.winfsp.is_ok(),
                mounted.is_some(),
                None,
            ));
            let _ = sub.append(&MenuItem::with_id(
                format!("open:{}", d.storage_id),
                "Open in Explorer",
                mounted.is_some(),
                None,
            ));
            add(&menu, &sub);
        }
        add(&menu, &PredefinedMenuItem::separator());
        add(
            &menu,
            &CheckMenuItem::with_id("pause", "Pause sync", true, a.paused.load(Ordering::Relaxed), None),
        );

        // Photo backup
        let pb = Submenu::new("Photo backup", true);
        let _ = pb.append(&CheckMenuItem::with_id(
            "backup:toggle",
            "Back up new photos",
            !a.drives.is_empty(),
            a.cfg.backup.enabled,
            None,
        ));
        let folder = a
            .backup_folder()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into());
        let _ = pb.append(&MenuItem::with_id(
            "backup:folder",
            format!("Folder: {folder}…"),
            true,
            None,
        ));
        let dsub = Submenu::new(
            format!(
                "Upload to: {}",
                a.backup_drive().map(|d| d.name.as_str()).unwrap_or("-")
            ),
            !a.drives.is_empty(),
        );
        let current = a.backup_drive().map(|d| d.storage_id.clone());
        for d in &a.drives {
            let _ = dsub.append(&CheckMenuItem::with_id(
                format!("backupdrive:{}", d.storage_id),
                &d.name,
                true,
                current.as_deref() == Some(d.storage_id.as_str()),
                None,
            ));
        }
        let _ = pb.append(&dsub);
        if let Some(b) = &a.backup {
            let st = b.status.lock();
            let line = match (&st.last_error, &st.last) {
                (Some(e), _) => format!("Last error: {}", e.chars().take(60).collect::<String>()),
                (None, Some(r)) => format!(
                    "{} backed up ({} new last pass)",
                    st.total_backed_up, r.uploaded
                ),
                _ => "Scanning…".into(),
            };
            let _ = pb.append(&MenuItem::with_id("backup:status", line, false, None));
        }
        add(&menu, &pb);

        if a.orgs.len() > 1 {
            let org = Submenu::new("Organization", true);
            for o in &a.orgs {
                let _ = org.append(&CheckMenuItem::with_id(
                    format!("org:{}", o.id),
                    &o.name,
                    true,
                    a.cfg.org_id.as_deref() == Some(o.id.as_str()),
                    None,
                ));
            }
            add(&menu, &org);
        }
        add(&menu, &PredefinedMenuItem::separator());
        add(&menu, &MenuItem::with_id("web", "Open OTHCloud in browser", true, None));
        add(&menu, &MenuItem::with_id("signout", "Sign out", true, None));
    } else if a.signed_in() {
        add(&menu, &MenuItem::with_id("retry", "Retry connecting", true, None));
        add(&menu, &MenuItem::with_id("signout", "Sign out", true, None));
    } else {
        add(&menu, &MenuItem::with_id("signin", "Sign in…", true, None));
    }
    add(&menu, &MenuItem::with_id("logs", "Open log folder", true, None));
    add(&menu, &PredefinedMenuItem::separator());
    add(&menu, &MenuItem::with_id("quit", "Quit", true, None));
    menu
}

fn tray_state(a: &App) -> u8 {
    if a.winfsp.is_err() || a.last_error.is_some() && a.api.is_none() {
        2
    } else if a.paused.load(Ordering::Relaxed) || !a.signed_in() {
        1
    } else {
        0
    }
}

fn handle_link(app: &Shared, proxy: &EventLoopProxy<UserEvent>, link: String) {
    let (code, origin) = match crate::config::parse_auth_url(&link) {
        Ok(x) => x,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Error(format!("Ignored link: {e}")));
            return;
        }
    };
    let trusted = app.lock().trusted_origin(&origin);
    if !trusted
        && !platform::confirm(
            "OTHCloud Drive",
            &format!(
                "Sign in to the OTHCloud panel at\n\n{origin}\n\nOnly continue if you just clicked \"Connect Windows app\" there."
            ),
        )
    {
        return;
    }
    work(app, proxy, move |a| a.sign_in_with_code(&code, &origin));
}

pub fn run(app: Shared, initial_link: Option<String>) {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let p = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(UserEvent::Menu(e));
    }));
    let p = proxy.clone();
    crate::ipc::serve(move |msg| {
        let ev = if msg == "quit" {
            UserEvent::Quit
        } else {
            UserEvent::Link(msg)
        };
        let _ = p.send_event(ev);
    });

    let mut tray: Option<TrayIcon> = None;
    let mut last_tip = String::new();
    let mut initial_link = initial_link;
    let mut last_check = Instant::now();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(5));
        let rebuild = |tray: &mut Option<TrayIcon>, last_tip: &mut String| {
            if let Some(a) = app.try_lock_for(Duration::from_millis(100)) {
                if let Some(t) = tray.as_ref() {
                    t.set_menu(Some(Box::new(build_menu(&a))));
                    let _ = t.set_icon(Some(icon(tray_state(&a))));
                    let tip = format!("OTHCloud Drive — {}", a.status_line());
                    if &tip != last_tip {
                        let _ = t.set_tooltip(Some(tip.clone()));
                        *last_tip = tip;
                    }
                }
            }
        };
        match event {
            Event::NewEvents(StartCause::Init) => {
                let menu = build_menu(&app.lock());
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_tooltip("OTHCloud Drive")
                    .with_icon(icon(1))
                    .build()
                    .map_err(|e| tracing::error!("tray: {e}"))
                    .ok();
                // Connect with stored credentials (or process the link we
                // were started with).
                if let Some(l) = initial_link.take() {
                    handle_link(&app, &proxy, l);
                } else if app.lock().signed_in() {
                    work(&app, &proxy, |a| a.connect());
                } else {
                    let _ = proxy.send_event(UserEvent::Refresh);
                }
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                rebuild(&mut tray, &mut last_tip);
                // Offline at start, or new drives on the account: retry /
                // refresh now and then.
                if last_check.elapsed() > Duration::from_secs(120) {
                    last_check = Instant::now();
                    if let Some(a) = app.try_lock() {
                        let (signed, connected) = (a.signed_in(), a.api.is_some());
                        drop(a);
                        if signed && !connected {
                            work(&app, &proxy, |a| a.connect());
                        } else if connected {
                            work(&app, &proxy, |a| {
                                a.refresh_drives()?;
                                a.apply_mounts();
                                Ok(())
                            });
                        }
                    }
                }
            }
            Event::UserEvent(UserEvent::Quit) => {
                tray.take();
                app.lock().shutdown();
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::Refresh) => rebuild(&mut tray, &mut last_tip),
            Event::UserEvent(UserEvent::Error(e)) => {
                tracing::warn!("{e}");
                rebuild(&mut tray, &mut last_tip);
            }
            Event::UserEvent(UserEvent::Link(l)) => handle_link(&app, &proxy, l),
            Event::UserEvent(UserEvent::Menu(ev)) => {
                let id = ev.id.0.clone();
                match id.as_str() {
                    "quit" => {
                        tray.take();
                        app.lock().shutdown();
                        *control_flow = ControlFlow::Exit;
                    }
                    "signin" => {
                        let o = app.lock().sign_in_origin();
                        platform::open_url(&format!("{o}/dashboard/apps/storage?connect=windows"));
                    }
                    "web" => {
                        let o = app.lock().sign_in_origin();
                        platform::open_url(&format!("{o}/dashboard/apps/storage"));
                    }
                    "signout" => work(&app, &proxy, |a| {
                        a.sign_out();
                        Ok(())
                    }),
                    "retry" => work(&app, &proxy, |a| {
                        a.last_error = None;
                        a.connect()
                    }),
                    "pause" => {
                        let mut a = app.lock();
                        let p = !a.paused.load(Ordering::Relaxed);
                        a.set_paused(p);
                        drop(a);
                        rebuild(&mut tray, &mut last_tip);
                    }
                    "logs" => {
                        let d = app.lock().data_dir.join("logs");
                        platform::open_in_explorer(&d.display().to_string());
                    }
                    "backup:toggle" => {
                        let on = !app.lock().cfg.backup.enabled;
                        work(&app, &proxy, move |a| {
                            a.set_backup_enabled(on);
                            Ok(())
                        });
                    }
                    "backup:folder" => {
                        let start = app.lock().backup_folder();
                        let mut dlg = rfd::FileDialog::new().set_title("Folder to back up");
                        if let Some(s) = start {
                            dlg = dlg.set_directory(s);
                        }
                        if let Some(f) = dlg.pick_folder() {
                            work(&app, &proxy, move |a| {
                                a.set_backup_folder(f);
                                Ok(())
                            });
                        }
                    }
                    other => {
                        if let Some(sid) = other.strip_prefix("mount:") {
                            let sid = sid.to_string();
                            let mounted = app.lock().mounts.contains_key(&sid);
                            work(&app, &proxy, move |a| {
                                a.last_error = None;
                                if mounted {
                                    a.disable(&sid);
                                    Ok(())
                                } else {
                                    a.mount(&sid).map(|_| ())
                                }
                            });
                        } else if let Some(sid) = other.strip_prefix("open:") {
                            if let Some(m) = app.lock().mounts.get(sid) {
                                platform::open_in_explorer(&format!("{}:\\", m.letter));
                            }
                        } else if let Some(sid) = other.strip_prefix("backupdrive:") {
                            let sid = sid.to_string();
                            work(&app, &proxy, move |a| {
                                a.set_backup_drive(&sid);
                                Ok(())
                            });
                        } else if let Some(org) = other.strip_prefix("org:") {
                            let org = org.to_string();
                            work(&app, &proxy, move |a| a.set_org(&org));
                        }
                    }
                }
            }
            _ => {}
        }
    });
}
