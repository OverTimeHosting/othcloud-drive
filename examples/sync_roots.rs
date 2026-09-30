//! List / clean up this user's OTHCloud Drive sync roots.
//!
//!   cargo run --example sync_roots                    list (id, path, policies)
//!   cargo run --example sync_roots -- unregister ID   unregister one (files untouched)
//!   cargo run --example sync_roots -- prune           unregister roots whose folder is gone

use othcloud_drive::cf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("unregister") => {
            let id = args.get(1).expect("id");
            match cf::unregister_sync_root(id) {
                Ok(()) => println!("unregistered {id}"),
                Err(e) => {
                    eprintln!("{id}: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("prune") => {
            for (id, p) in cf::our_sync_roots() {
                if p.as_os_str().is_empty() || !p.exists() {
                    match cf::unregister_sync_root(&id) {
                        Ok(()) => println!("pruned {id} ({})", p.display()),
                        Err(e) => eprintln!("{id}: {e}"),
                    }
                }
            }
        }
        _ => {
            for (id, p) in cf::our_sync_roots() {
                let pol = cf::root_policies(&p).map(|x| format!("{x:?}")).unwrap_or_else(|e| e.to_string());
                println!("{id}\t{}\t{pol}", p.display());
            }
        }
    }
}
