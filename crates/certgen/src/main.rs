//! Writes the internal-TLS certificate bundle. Run from the workspace root:
//!
//! ```text
//! cargo run -p rustibia-certgen
//! ```
//!
//! Both processes default to reading `certs/` relative to their own working directory,
//! so from `crates/site` or `crates/server` pass the path explicitly:
//! `cargo run -p rustibia-certgen -- ../../certs`.
//!
//! Regenerating invalidates nothing but itself — no certificate here is trusted by
//! anything outside this repository — but it does require restarting both processes,
//! since each loads its files once at boot.

use anyhow::Result;

fn main() -> Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "certs".to_string());

    rustibia_certgen::generate_bundle(&dir)?;

    println!("wrote the CA, site, server and game certificates to {dir}/");
    println!("keys are secrets: {dir}/ is git-ignored and must stay that way");
    Ok(())
}
