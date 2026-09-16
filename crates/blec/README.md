# blec

A cross platform BLE (Bluetooth Low Energy) client for rust.

On Linux, macOS, Windows and iOS this is a thin layer over
[btleplug](https://github.com/deviceplug/btleplug); on Android it uses its own backend.

For a Tauri app use [`tauri-plugin-blec`](../tauri-plugin-blec) instead, which wraps this crate
in a plugin with a JavaScript API and re-exports the whole rust API.

## Usage

```rust
use uuid::{uuid, Uuid};
use blec::models::{ScanFilter, WriteType};
use blec::OnDisconnectHandler;

const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");

async fn example() -> Result<(), blec::Error> {
    let handler = blec::init().await?;
    handler.discover(None, 1000, ScanFilter::None, false).await?;
    handler
        .connect("00:00:00:00:00:00", OnDisconnectHandler::None, false)
        .await?;
    handler
        .send_data(CHARACTERISTIC_UUID, None, &[1, 2, 3], WriteType::WithResponse)
        .await?;
    Ok(())
}
```

`blec::init()` creates the handler once; `blec::get_handler()` returns it from anywhere afterwards.

## Mock backend

The `mock` cargo feature (desktop only) replaces the platform backend with an in-process
simulation (`blec::mock`) that an app or a test can script: devices appear and disappear, links
drop, operations fail, hang or answer slowly, notifications arrive. See the handler tests in
`src/handler/tests`.

## License

MIT or Apache-2.0, at your option.
