use tracing::info;
use uuid::{uuid, Uuid};

const SERVICE_UUID: uuid::Uuid = uuid::uuid!("A07498CA-AD5B-474E-940D-16F1FBE7E8CD");
const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");

// command to test the BLE communication from rust
#[tauri::command]
async fn test() -> bool {
    const DATA: [u8; 500] = [0; 500];
    let handler = tauri_plugin_blec::get_handler().unwrap();
    let start = std::time::Instant::now();
    handler
        .send_data(
            CHARACTERISTIC_UUID,
            None,
            &DATA,
            tauri_plugin_blec::models::WriteType::WithoutResponse,
        )
        .await
        .unwrap();
    let response = handler
        .recv_data(CHARACTERISTIC_UUID, Some(SERVICE_UUID))
        .await
        .unwrap();
    let time = start.elapsed();
    info!("Time elapsed: {:?}", time);
    assert_eq!(response, DATA);
    true
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
#[allow(clippy::missing_panics_doc)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::new().build())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_blec::init())
        .invoke_handler(tauri::generate_handler![test])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
