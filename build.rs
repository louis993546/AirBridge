use std::fs;
use std::path::Path;

fn main() {
    embuild::espidf::sysenv::output();

    // Optional local device configuration in `cfg.toml` (gitignored).
    // If absent, the firmware compiles cleanly with generic placeholders.
    let cfg_path = Path::new("cfg.toml");
    println!("cargo:rerun-if-changed=cfg.toml");

    if let Ok(contents) = fs::read_to_string(cfg_path) {
        for raw_line in contents.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.trim().trim_matches('"');
                if !key.is_empty() {
                    println!("cargo:rustc-env=AIRBRIDGE_{}={}", key, val);
                }
            }
        }
    }
}
