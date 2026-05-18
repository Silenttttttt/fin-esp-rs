use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use log::{info, warn};
use crate::screen::UiState;

/// One-shot action flags set by the web UI, drained by the main loop.
pub struct WebTriggers {
    pub lamp:    AtomicBool,
    pub screen:  AtomicBool,
    pub display: AtomicBool,
    pub warm:    AtomicBool,
    pub bright:  AtomicBool,
    pub pot:     AtomicBool,
    pub media:   AtomicBool,
}

impl WebTriggers {
    pub fn new() -> Self {
        Self {
            lamp:    AtomicBool::new(false),
            screen:  AtomicBool::new(false),
            display: AtomicBool::new(false),
            warm:    AtomicBool::new(false),
            bright:  AtomicBool::new(false),
            pot:     AtomicBool::new(false),
            media:   AtomicBool::new(false),
        }
    }
}

const HTML: &[u8] = br#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Fin-ESP</title>
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:system-ui,sans-serif;background:#0d1117;color:#cdd9e5;padding:1rem;max-width:420px;margin:0 auto}
h2{text-align:center;color:#79c0ff;margin:.5rem 0 1rem;font-size:1.3rem}
.st{background:#161b22;border:1px solid #30363d;border-radius:.5rem;padding:.75rem 1rem;margin-bottom:1rem;font-size:.85rem;line-height:1.9}
.st b{color:#e3b341}
.grid{display:grid;grid-template-columns:1fr 1fr;gap:.5rem}
button{padding:.8rem .5rem;border:none;border-radius:.5rem;font-size:.9rem;font-weight:600;cursor:pointer;transition:filter .1s;width:100%}
button:active{filter:brightness(.75)}
.lamp{background:#e09000;color:#000}
.warm{background:#c94000;color:#fff}
.bright{background:#d8d8d8;color:#111}
.screen{background:#1060c8;color:#fff}
.display{background:#3a3a3a;color:#fff}
.pot{background:#1a6020;color:#fff}
.media{background:#7020b0;color:#fff}
</style>
</head>
<body>
<h2>Fin-ESP</h2>
<div class="st" id="s">Loading...</div>
<div class="grid">
<button class="lamp"    onclick="p('/action/lamp')">Lamp Toggle</button>
<button class="warm"    onclick="p('/action/warm')">Warm Dim</button>
<button class="bright"  onclick="p('/action/bright')">Bright White</button>
<button class="screen"  onclick="p('/action/screen')">Next Screen</button>
<button class="display" onclick="p('/action/display')">Display Toggle</button>
<button class="pot"     onclick="p('/action/pot')">Pot Toggle</button>
<button class="media"   onclick="p('/action/media')">Play / Pause</button>
</div>
<script>
function p(u){fetch(u,{method:'POST'}).then(refresh).catch(function(){})}
function refresh(){
  fetch('/status').then(function(r){return r.json()}).then(function(d){
    document.getElementById('s').innerHTML=
      'Screen: <b>'+d.screen+'</b><br>'+
      'Lamp: <b>'+(d.lamp_on?(d.lamp_known?'ON':'ON?'):(d.lamp_known?'OFF':'OFF?'))+'</b>'+
      ' &nbsp; Display: <b>'+(d.display_on?'ON':'OFF')+'</b><br>'+
      'Pot: <b>'+(d.pot_on?'enabled':'disabled')+'</b>';
  }).catch(function(){});
}
refresh();
setInterval(refresh,5000);
</script>
</body>
</html>"#;

fn write_response(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let hdr = std::format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status, ctype, body.len()
    );
    let _ = s.write_all(hdr.as_bytes());
    let _ = s.write_all(body);
}

fn drain_headers(r: &mut BufReader<TcpStream>) {
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) if line.trim_end().is_empty() => return,
            _ => {}
        }
    }
}

fn handle(
    stream: TcpStream,
    triggers: &WebTriggers,
    ui_state: &Arc<Mutex<UiState>>,
    screen_forced_off: &Arc<AtomicBool>,
) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let read_clone = match stream.try_clone() { Ok(s) => s, Err(_) => return };
    let mut reader = BufReader::new(read_clone);

    let mut first_line = String::new();
    if reader.read_line(&mut first_line).is_err() { return; }
    drain_headers(&mut reader);

    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 { return; }
    let mut s = stream;

    match (parts[0], parts[1]) {
        ("GET", "/") => {
            write_response(&mut s, "200 OK", "text/html; charset=utf-8", HTML);
        }
        ("GET", "/status") => {
            let json = {
                let st = ui_state.lock().unwrap();
                std::format!(
                    r#"{{"screen":"{}","lamp_on":{},"lamp_known":{},"display_on":{},"pot_on":{}}}"#,
                    st.screen.name().trim(),
                    st.lamp.on, st.lamp.known,
                    !screen_forced_off.load(Ordering::Relaxed),
                    st.pot_enabled,
                )
            };
            write_response(&mut s, "200 OK", "application/json", json.as_bytes());
        }
        ("POST", "/action/lamp")    => { triggers.lamp.store(true,    Ordering::Relaxed); write_response(&mut s, "200 OK", "text/plain", b"ok"); }
        ("POST", "/action/screen")  => { triggers.screen.store(true,  Ordering::Relaxed); write_response(&mut s, "200 OK", "text/plain", b"ok"); }
        ("POST", "/action/display") => { triggers.display.store(true, Ordering::Relaxed); write_response(&mut s, "200 OK", "text/plain", b"ok"); }
        ("POST", "/action/warm")    => { triggers.warm.store(true,    Ordering::Relaxed); write_response(&mut s, "200 OK", "text/plain", b"ok"); }
        ("POST", "/action/bright")  => { triggers.bright.store(true,  Ordering::Relaxed); write_response(&mut s, "200 OK", "text/plain", b"ok"); }
        ("POST", "/action/pot")     => { triggers.pot.store(true,     Ordering::Relaxed); write_response(&mut s, "200 OK", "text/plain", b"ok"); }
        ("POST", "/action/media")   => { triggers.media.store(true,   Ordering::Relaxed); write_response(&mut s, "200 OK", "text/plain", b"ok"); }
        _ => { write_response(&mut s, "404 Not Found", "text/plain", b"not found"); }
    }
}

pub fn spawn(
    triggers: Arc<WebTriggers>,
    ui_state: Arc<Mutex<UiState>>,
    screen_forced_off: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("web-srv".into())
        .stack_size(8192)
        .spawn(move || {
            loop {
                let listener = match TcpListener::bind("0.0.0.0:80") {
                    Ok(l) => l,
                    Err(e) => {
                        warn!("[web] bind failed: {} — retrying", e);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                info!("[web] listening on :80");
                for stream in listener.incoming() {
                    match stream {
                        Ok(s) => handle(s, &triggers, &ui_state, &screen_forced_off),
                        Err(e) => { warn!("[web] accept err: {}", e); break; }
                    }
                }
            }
        })
        .ok();
}
