use std::{env, error::Error, fs, path::PathBuf};

/// The AES key that unwraps `key.enc` is compiled into the binary from a file that is
/// deliberately kept out of version control. Builds without it still succeed; edit
/// predictions just report that the bundled credentials are unavailable.
fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let secrets_dir = manifest_dir.join("..").join("..").join("secrets");
    let wrap_key_path = secrets_dir.join("key-wrap.hex");
    println!("cargo:rerun-if-changed={}", wrap_key_path.display());

    let wrap_key = match fs::read_to_string(&wrap_key_path) {
        Ok(text) => Some(
            parse_wrap_key(text.trim())
                .ok_or("secrets/key-wrap.hex must contain exactly 64 hex characters")?,
        ),
        Err(_) => {
            println!(
                "cargo:warning=secrets/key-wrap.hex not found; DeepSeek edit predictions will be unavailable in this build"
            );
            None
        }
    };

    let literal = match wrap_key {
        Some(bytes) => format!("Some({bytes:?})"),
        None => "None".to_string(),
    };
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    fs::write(
        out_dir.join("key_wrap.rs"),
        format!("const KEY_WRAP: Option<[u8; 32]> = {literal};\n"),
    )?;
    Ok(())
}

fn parse_wrap_key(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}
