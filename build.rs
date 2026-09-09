const COMMANDS: &[&str] = &[
    "scan",
    "stop_scan",
    "connect",
    "disconnect",
    "connection_state",
    "send",
    "send_string",
    "recv",
    "recv_string",
    "subscribe",
    "subscribe_string",
    "unsubscribe",
    "scanning_state",
    "check_permissions",
    "list_services",
    "get_adapter_state",
    "mtu",
    "set_write_behavior",
    "set_timeouts",
    "set_android_mtu",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .build();
}
