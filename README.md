# blec

A cross platform BLE client for rust, and a Tauri plugin built on it.

| Crate | Description |
| --- | --- |
| [`crates/blec`](crates/blec) | The BLE client itself: handler, models, platform backends, mock. Host agnostic. |
| [`crates/tauri-plugin-blec`](crates/tauri-plugin-blec) | Tauri plugin wrapping `blec`, with the `@mnlphlp/plugin-blec` JavaScript API. |

On Linux, macOS, Windows and iOS the backend is
[btleplug](https://github.com/deviceplug/btleplug); Android has its own backend.

## Which one do I want?

- Tauri app → `tauri-plugin-blec` (it re-exports all of `blec`, so you do not need both).
- Any other rust app (Dioxus, egui, a CLI, a daemon) → `blec`.

## Examples

- [examples/plugin-blec-example](examples/plugin-blec-example) — Tauri + Vue app: scan, connect, send/receive.
- [examples/test-server](examples/test-server) — GATT peripheral to test against (Linux/BlueZ).
- [examples/stress-server](examples/stress-server) + [examples/stress-client](examples/stress-client) —
  scripted failure scenarios for the full stack, see [examples/stress-protocol.md](examples/stress-protocol.md).

## Development

```bash
cargo test --workspace                 # handler tests through the mock backend
cargo check --workspace --features tauri-plugin-blec/mock
cargo check --workspace --target aarch64-linux-android
```

The examples are standalone crates with their own lockfiles and are not workspace members.

## License

MIT or Apache-2.0, at your option.
