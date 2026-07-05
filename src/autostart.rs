//! "Start with Windows" via the per-user Run registry key. No admin needed.

use anyhow::{Context, Result};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Floaty";

pub fn is_enabled() -> bool {
    windows_registry::CURRENT_USER
        .open(RUN_KEY)
        .and_then(|k| k.get_string(VALUE_NAME))
        .is_ok()
}

pub fn set_enabled(enable: bool) -> Result<()> {
    let key = windows_registry::CURRENT_USER
        .create(RUN_KEY)
        .context("opening HKCU Run key")?;
    if enable {
        let exe = std::env::current_exe().context("resolving current exe path")?;
        // Quote the path: it may contain spaces.
        key.set_string(VALUE_NAME, format!("\"{}\"", exe.display()))
            .context("writing Run value")?;
    } else if key.get_string(VALUE_NAME).is_ok() {
        key.remove_value(VALUE_NAME).context("removing Run value")?;
    }
    Ok(())
}

/// Make the registry agree with the config at startup and after config edits.
pub fn sync(want: bool) {
    if is_enabled() != want {
        if let Err(e) = set_enabled(want) {
            log::error!("autostart sync failed: {e:#}");
        } else {
            log::info!("autostart set to {want}");
        }
    }
}
