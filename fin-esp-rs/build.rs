fn main() {
    embuild::espidf::sysenv::output();

    // Load secrets from .env so they never have to live in source code.
    // Each non-blank, non-comment line becomes a cargo:rustc-env variable
    // accessible via env!("KEY") in the crate at compile time.
    let env_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    if let Ok(contents) = std::fs::read_to_string(&env_path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((key, val)) = line.split_once('=') {
                println!("cargo:rustc-env={}={}", key.trim(), val.trim());
            }
        }
    }

    // Re-run build script whenever .env changes.
    println!("cargo:rerun-if-changed=.env");
}
