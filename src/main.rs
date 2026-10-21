// Release builds are a GUI (tray) app: no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! OTHCloud Drive.
//!
//!   othcloud-drive                         tray app (single instance)
//!   othcloud-drive "othcloud-drive://auth?code=…&origin=…"
//!                                          sign-in link (hands it to the
//!                                          running instance if there is one)
//!   othcloud-drive quit                    ask the running instance to quit
//!   othcloud-drive mount --origin URL --token T --org ORG --storage ID
//!                  [--storage ID…] [--root DIR…] [--letter L…] [--no-letter]
//!                  [--poll-secs N] [--stop-file PATH] [--remove-on-exit]
//!                  [--provider-id ID] [--cover-secs N]
//!                                          headless sync (testing/scripting)
//!   othcloud-drive register                (re)register the URL scheme
//!   othcloud-drive cleanup-test-roots      dev: remove test sync roots/letters

use othcloud_drive::{api, app, engine, ipc, platform, tray};
use parking_lot::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

fn init_logging(console: bool) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::prelude::*;
    let dir = platform::data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("othcloud-drive")
        .filename_suffix("log")
        .max_log_files(14)
        .build(&dir)
        .expect("log file");
    let (nb, guard) = tracing_appender::non_blocking(file);
    let filter = tracing_subscriber::EnvFilter::try_from_env("OTHCLOUD_DRIVE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let file_layer = tracing_subscriber::fmt::layer().with_writer(nb).with_ansi(false);
    let console_layer = console.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false)
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console_layer)
        .init();
    guard
}

/// Release builds have no console; attach to the parent's (if any) so
/// command-line modes can print.
fn attach_console() {
    #[cfg(not(debug_assertions))]
    unsafe {
        use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn arg_values(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == name && i + 1 < args.len() {
            out.push(args[i + 1].clone());
            i += 1;
        }
        i += 1;
    }
    out
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    arg_values(args, name).into_iter().next()
}

fn headless_mount(args: &[String]) -> i32 {
    let need = |n: &str| {
        arg_value(args, n).unwrap_or_else(|| {
            eprintln!("missing {n}");
            std::process::exit(2)
        })
    };
    let origin = match api::normalize_origin(&need("--origin")) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let token = arg_value(args, "--token")
        .or_else(|| std::env::var("OTHCLOUD_DRIVE_TOKEN").ok())
        .unwrap_or_else(|| need("--token"));
    let org = need("--org");
    let storages = arg_values(args, "--storage");
    let letters_arg: Vec<char> = arg_values(args, "--letter")
        .iter()
        .filter_map(|l| l.chars().next())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let roots_arg = arg_values(args, "--root");
    let no_letter = args.iter().any(|a| a == "--no-letter");
    let remove = args.iter().any(|a| a == "--remove-on-exit");
    let provider = arg_value(args, "--provider-id").unwrap_or_else(|| othcloud_drive::cf::PROVIDER.to_string());
    let cover_secs = arg_value(args, "--cover-secs").and_then(|s| s.parse().ok()).unwrap_or(15u64);
    let poll = arg_value(args, "--poll-secs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(15u64);
    let stop_file = arg_value(args, "--stop-file").map(std::path::PathBuf::from);

    let api = api::Api::new(&origin, &token, Some(org));
    let drives = match api.drives() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("listing drives: {e}");
            return 4;
        }
    };
    let paused = Arc::new(AtomicBool::new(false));
    let data_dir = platform::data_dir();
    let mut mounts = Vec::new();
    let mut used: Vec<char> = Vec::new();
    let mut cfg = othcloud_drive::config::Config::default();
    for (i, sid) in storages.iter().enumerate() {
        let Some(d) = drives.iter().find(|d| &d.storage_id == sid) else {
            eprintln!(
                "no drive {sid} (have: {:?})",
                drives.iter().map(|d| &d.storage_id).collect::<Vec<_>>()
            );
            return 4;
        };
        let root = match roots_arg.get(i) {
            Some(r) => std::path::PathBuf::from(r),
            None => app::root_for(&mut cfg, d),
        };
        let opts = engine::EngineOptions {
            poll_interval: Duration::from_secs(poll),
            provider: provider.clone(),
            cover_interval: Duration::from_secs(cover_secs),
            ..Default::default()
        };
        match app::mount_drive(
            api.clone(),
            d,
            &root,
            letters_arg.get(i).copied(),
            !no_letter,
            &used,
            &data_dir,
            paused.clone(),
            opts,
        ) {
            Ok(m) => {
                let l = m.letter.map(|l| format!("{l}:")).unwrap_or_else(|| "-".into());
                println!("MOUNTED {} {} {}", sid, l, m.root.display());
                if let Some(l) = m.letter {
                    used.push(l);
                }
                mounts.push(m);
            }
            Err(e) => {
                eprintln!("mount {sid}: {e}");
                for m in mounts {
                    let _ = app::unmount_drive(m, remove, &app::roots_base());
                }
                return 6;
            }
        }
    }
    // Wait for: the stop file, "quit" on stdin, or stdin EOF (only when a
    // stop file wasn't given — a detached process may have no stdin).
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    if stop_file.is_none() {
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match std::io::stdin().read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.trim() == "quit" => break,
                    Ok(_) => {}
                }
            }
            let _ = tx2.send(());
        });
    }
    loop {
        if rx.recv_timeout(Duration::from_millis(300)).is_ok() {
            break;
        }
        if let Some(f) = &stop_file {
            if f.exists() {
                let _ = std::fs::remove_file(f);
                break;
            }
        }
    }
    let mut code = 0;
    for m in mounts {
        match app::unmount_drive(m, remove, &app::roots_base()) {
            Ok(Some(aside)) => println!("NOT-UPLOADED-MOVED-TO {}", aside.display()),
            Ok(None) => {}
            Err(e) => {
                eprintln!("{e}");
                code = 7;
            }
        }
    }
    println!("UNMOUNTED{}", if remove { " REMOVED" } else { "" });
    code
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(|s| s.as_str()).unwrap_or("");

    if first == "mount" {
        attach_console();
        let _g = init_logging(true);
        std::process::exit(headless_mount(&args[1..]));
    }
    if first == "cleanup-test-roots" {
        attach_console();
        for line in cleanup_test_roots() {
            println!("{line}");
        }
        return;
    }
    if first == "register" {
        attach_console();
        let exe = std::env::current_exe().expect("exe path");
        match platform::register_url_scheme(&exe) {
            Ok(()) => println!("registered {}:// -> {}", platform::URL_SCHEME, exe.display()),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if first == "--version" || first == "-V" {
        attach_console();
        println!("othcloud-drive {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let link = first
        .to_ascii_lowercase()
        .starts_with(&format!("{}:", platform::URL_SCHEME))
        .then(|| first.to_string());

    let Some(_lock) = ipc::acquire_instance() else {
        // Another instance runs: hand over the link / command and leave.
        let msg = if first == "quit" {
            Some("quit".to_string())
        } else {
            link
        };
        if let Some(m) = msg {
            if let Err(e) = ipc::send(&m) {
                platform::message_box("OTHCloud Drive", &format!("Couldn't reach the running OTHCloud Drive: {e}"), true);
            }
        }
        return;
    };
    if first == "quit" {
        return; // nothing running
    }

    let _g = init_logging(cfg!(debug_assertions));
    tracing::info!("OTHCloud Drive {} starting", env!("CARGO_PKG_VERSION"));
    if let Ok(exe) = std::env::current_exe() {
        if let Err(e) = platform::register_url_scheme(&exe) {
            tracing::warn!("registering the URL scheme: {e}");
        }
    }
    let app = app::App::load();
    let app = Arc::new(Mutex::new(app));
    tray::run(app, link);
}

/// Dev/test only: remove every sync root registered under the TEST provider
/// id (never the app's own), any drive letter aliasing into one (with its
/// Explorer label/icon keys), and the root folder itself if it's under %TEMP%.
fn cleanup_test_roots() -> Vec<String> {
    use othcloud_drive::{cf, letters};
    let mut out = Vec::new();
    let temp = std::env::temp_dir().display().to_string().to_lowercase();
    for (id, root) in cf::sync_roots(cf::TEST_PROVIDER) {
        let rs = root.display().to_string().to_lowercase();
        if !rs.is_empty() {
            for l in 'A'..='Z' {
                if let Some(t) = letters::root_of(l) {
                    let ts = t.display().to_string().to_lowercase();
                    if ts == rs || ts.starts_with(&format!("{rs}\\")) {
                        letters::unmap(l, &t);
                        platform::remove_drive_icon(l);
                        out.push(format!("removed letter {l}: -> {}", t.display()));
                    }
                }
            }
        }
        match cf::unregister_sync_root(&id) {
            Ok(()) => out.push(format!("unregistered {id}")),
            Err(e) => out.push(format!("{id}: {e}")),
        }
        if !rs.is_empty() && rs.starts_with(&temp) && root.exists() {
            match std::fs::remove_dir_all(&root) {
                Ok(()) => out.push(format!("deleted {}", root.display())),
                Err(e) => out.push(format!("{}: {e}", root.display())),
            }
        }
    }
    if out.is_empty() {
        out.push("no test sync roots".into());
    }
    out
}
