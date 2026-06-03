// Tauri 2 build glue: reads tauri.conf.json + capabilities and emits
// the generated context that `tauri::generate_context!()` consumes in main.rs.
fn main() {
    tauri_build::build()
}
