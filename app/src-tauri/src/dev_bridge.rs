//! Development only: expose the app's commands over HTTP on localhost so the UI can be driven
//! from an ordinary browser (automated UI checks, screenshots). Enabled by SCRUBLESS_DEV_BRIDGE=<port>
//! and never compiled into release builds.

use serde_json::{json, Value};
use tauri::AppHandle;

fn arg<T: serde::de::DeserializeOwned>(a: &Value, k: &str) -> Result<T, String> {
    serde_json::from_value(a.get(k).cloned().unwrap_or(Value::Null)).map_err(|e| format!("arg {k}: {e}"))
}

fn v<T: serde::Serialize>(x: Result<T, String>) -> Result<Value, String> {
    x.map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
}

fn dispatch(h: &AppHandle, cmd: &str, a: &Value) -> Result<Value, String> {
    use super::*;
    let st = || h.state::<App>();
    match cmd {
        "status" => v(status(st())),
        "add_folder" => v(add_folder(st(), arg(a, "path")?)),
        "remove_folder" => v(remove_folder(st(), arg(a, "path")?)),
        "rescan" => v(rescan(st())),
        "is_dir" => Ok(json!(is_dir(arg(a, "path")?))),
        "set_paused" => {
            set_paused(st(), arg(a, "paused")?);
            Ok(Value::Null)
        }
        "set_transcribe" => v(set_transcribe(st(), arg(a, "on")?)),
        "get_settings" => v(Ok(get_settings(st()))),
        "set_settings" => v(set_settings(st(), arg(a, "settings")?)),
        "retranscribe_all" => v(retranscribe_all(st())),
        "clear_cache" => v(clear_cache(st())),
        "search" => v(tauri::async_runtime::block_on(search(st(), arg(a, "query")?, arg(a, "filters")?))),
        "search_by_file" => v(tauri::async_runtime::block_on(search_by_file(st(), arg(a, "path")?, arg(a, "filters")?))),
        "search_similar" => v(tauri::async_runtime::block_on(search_similar(st(), arg(a, "itemId")?, arg(a, "filters")?))),
        "transcript" => v(Ok(transcript(st(), arg(a, "fileId")?))),
        "file_info" => v(Ok(file_info(st(), arg(a, "fileId")?))),
        "preview" => v(tauri::async_runtime::block_on(preview(st(), arg(a, "path")?, arg(a, "t0")?, arg(a, "t1")?))),
        "export_clip" => v(tauri::async_runtime::block_on(export_clip(st(), arg(a, "path")?, arg(a, "t0")?, arg(a, "t1")?, arg(a, "pad")?, arg(a, "dest")?))),
        "add_select" => v(add_select(st(), arg(a, "fileId")?, arg(a, "t0")?, arg(a, "t1")?, arg(a, "note")?)),
        "remove_select" => v(remove_select(st(), arg(a, "id")?)),
        "clear_selects" => v(clear_selects(st())),
        "selects" => v(selects(st())),
        "export_selects" => v(tauri::async_runtime::block_on(export_selects(st(), arg(a, "format")?, arg(a, "dest")?, arg(a, "pad")?))),
        _ => Err(format!("unknown command {cmd}")),
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

pub fn start(h: AppHandle, port: u16) {
    std::thread::spawn(move || {
        let server = match tiny_http::Server::http(("127.0.0.1", port)) {
            Ok(s) => s,
            Err(e) => return eprintln!("dev bridge: {e}"),
        };
        eprintln!("dev bridge on http://127.0.0.1:{port}");
        for mut req in server.incoming_requests() {
            let h = h.clone();
            std::thread::spawn(move || {
                let cors = tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap();
                let url = req.url().to_string();
                if let Some(cmd) = url.strip_prefix("/invoke/") {
                    let mut body = String::new();
                    let _ = req.as_reader().read_to_string(&mut body);
                    let args: Value = serde_json::from_str(&body).unwrap_or(json!({}));
                    let (code, out) = match dispatch(&h, cmd, &args) {
                        Ok(v) => (200, v),
                        Err(e) => (500, json!(e)),
                    };
                    let ct = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
                    let _ = req.respond(tiny_http::Response::from_string(out.to_string()).with_status_code(code).with_header(cors).with_header(ct));
                } else if let Some(q) = url.strip_prefix("/file?path=") {
                    let path = urlencoding::decode(q).map(|s| s.into_owned()).unwrap_or_default();
                    match std::fs::File::open(&path) {
                        Ok(f) => {
                            let ct = tiny_http::Header::from_bytes("Content-Type", content_type(&path)).unwrap();
                            let _ = req.respond(tiny_http::Response::from_file(f).with_header(cors).with_header(ct));
                        }
                        Err(_) => {
                            let _ = req.respond(tiny_http::Response::empty(404).with_header(cors));
                        }
                    }
                } else if req.method() == &tiny_http::Method::Options {
                    let hdr = tiny_http::Header::from_bytes("Access-Control-Allow-Headers", "*").unwrap();
                    let _ = req.respond(tiny_http::Response::empty(204).with_header(cors).with_header(hdr));
                } else {
                    let _ = req.respond(tiny_http::Response::empty(404).with_header(cors));
                }
            });
        }
    });
}
