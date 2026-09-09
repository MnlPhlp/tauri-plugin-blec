//! Startup check of the BlueZ configuration.
//!
//! bluetoothd runs GATT service discovery on every central that connects, which
//! opens an ATT client channel to the phone. Android then counts its own GATT
//! server as a holder of the radio link, and a client-initiated disconnect no
//! longer drops the ACL (see README, "Requirements"). The stress scripts need
//! real link drops, so `[GATT] Client = false` is required. BlueZ does not
//! expose the setting over D-Bus; the only way to verify it is the config file.

use std::{fs, path::Path};

use anyhow::{anyhow, Result};

pub const MAIN_CONF: &str = "/etc/bluetooth/main.conf";
const DROP_IN_DIR: &str = "/etc/bluetooth/main.conf.d";

/// The state of `[GATT] Client` in the BlueZ configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GattClient {
    /// `Client = false` is set.
    Disabled,
    /// `Client = true` is set explicitly, or the key is absent (the default is true).
    Enabled { explicit: bool },
}

/// Reads `/etc/bluetooth/main.conf` (and `main.conf.d/*.conf`, later files win)
/// and reports whether BlueZ's GATT client role is disabled.
pub fn gatt_client_setting() -> Result<GattClient> {
    let mut files = vec![Path::new(MAIN_CONF).to_path_buf()];
    if let Ok(dir) = fs::read_dir(DROP_IN_DIR) {
        let mut drop_ins: Vec<_> = dir
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "conf"))
            .collect();
        drop_ins.sort();
        files.extend(drop_ins);
    }

    let mut setting: Option<bool> = None;
    let mut read_any = false;
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        read_any = true;
        if let Some(value) = parse_gatt_client(&text) {
            setting = Some(value);
        }
    }
    if !read_any {
        return Err(anyhow!("cannot read {MAIN_CONF}"));
    }
    Ok(match setting {
        Some(false) => GattClient::Disabled,
        Some(true) => GattClient::Enabled { explicit: true },
        None => GattClient::Enabled { explicit: false },
    })
}

/// Returns the value of `Client` in the `[GATT]` section, if present.
fn parse_gatt_client(text: &str) -> Option<bool> {
    let mut in_gatt = false;
    let mut value = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            in_gatt = line.trim_end_matches(']').trim_start_matches('[').trim().eq_ignore_ascii_case("GATT");
            continue;
        }
        if !in_gatt {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("Client") {
            value = match val.trim().to_ascii_lowercase().as_str() {
                "false" | "no" | "0" => Some(false),
                "true" | "yes" | "1" => Some(true),
                _ => value,
            };
        }
    }
    value
}

/// Human readable explanation and fix for an enabled GATT client role.
pub fn enabled_message(setting: &GattClient) -> String {
    let how = match setting {
        GattClient::Enabled { explicit: true } => "`Client = true` is set",
        _ => "`Client` is not set (default true)",
    };
    format!(
        "BlueZ's GATT client role is enabled ({how} in the [GATT] section of {MAIN_CONF}). \
         bluetoothd will run service discovery on every connecting central; on Android the \
         phone then keeps the radio link after the app disconnects and the disconnect \
         scripts cannot pass. Set\n\n    [GATT]\n    Client = false\n\n\
         in {MAIN_CONF}, run `sudo systemctl restart bluetooth`, and start the server again. \
         Use --ignore-bluez-config to run anyway."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_setting() {
        assert_eq!(parse_gatt_client("[GATT]\nClient = false\n"), Some(false));
        assert_eq!(parse_gatt_client("[General]\nClient = false\n"), None);
        assert_eq!(parse_gatt_client("[GATT]\n#Client = false\n"), None);
        assert_eq!(parse_gatt_client("[GATT]\nCache = always\nClient=true\n[Policy]\n"), Some(true));
        assert_eq!(parse_gatt_client("[gatt]\n client = No \n"), Some(false));
    }
}
