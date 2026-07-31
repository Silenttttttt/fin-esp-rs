use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI8, Ordering}};
use std::time::Duration;
use log::{info, warn};
use crate::led::LedState;
use crate::screen::UiState;
use crate::tuya::{LampHandle, LAMP_UNKNOWN};


/// Triggers handled by the main loop (require LCD render / persist / atomics).
pub struct WebTriggers {
    pub screen:        AtomicBool,
    pub screen_select: AtomicI8,  // -1 = none, 0-4 = direct
    pub display:       AtomicI8,  // -1 = none, 0 = set off, 1 = set on
    pub pot:           AtomicI8,  // -1 = none, 0 = set off, 1 = set on
    pub media:         AtomicBool,
    pub volume:        AtomicI8,  // -1 = none, 0-100 = set volume pct
}

impl WebTriggers {
    pub fn new() -> Self {
        Self {
            screen:        AtomicBool::new(false),
            screen_select: AtomicI8::new(-1),
            display:       AtomicI8::new(-1),
            pot:           AtomicI8::new(-1),
            media:         AtomicBool::new(false),
            volume:        AtomicI8::new(-1),
        }
    }
}

// ── HTML ──────────────────────────────────────────────────────────────────────
const HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Fin-ESP</title>
<style>
:root{--bg:#07070f;--s1:#0e0e1b;--s2:#131326;--bd:rgba(255,255,255,.07);
  --acc:#8b5cf6;--acc2:#6d28d9;--up:#22c55e;--dn:#ef4444;
  --txt:#e2e2f0;--dim:#5a5a9a;--r:.85rem}
*{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--txt);font:var(--r)/1.5 system-ui,sans-serif;
  max-width:500px;margin:0 auto;padding:1rem .85rem 2rem}
.hdr{display:flex;align-items:center;gap:.6rem;margin-bottom:.9rem;padding:.2rem 0}
h1{font-size:1rem;font-weight:700;color:var(--acc);letter-spacing:.02em;flex-shrink:0}
.clk{font-size:.82rem;font-variant-numeric:tabular-nums;color:var(--txt);margin-right:auto}
.wth{font-size:.73rem;color:var(--dim);white-space:nowrap}
.ft{font-size:.72rem;color:var(--dim);letter-spacing:.05em}
.dot{width:8px;height:8px;border-radius:50%;background:#2a2a4a;
  transition:background .4s,box-shadow .4s;flex-shrink:0}
.dot.ok{background:var(--up);box-shadow:0 0 8px rgba(34,197,94,.6)}
.card{background:var(--s1);border:1px solid var(--bd);border-radius:.9rem;
  padding:1.1rem 1rem;margin-bottom:.6rem;transition:box-shadow .5s}
.card.glow{box-shadow:0 0 50px -14px rgba(249,115,22,.3),
  inset 0 1px 0 rgba(255,255,255,.04)}
.ch{display:flex;align-items:center;gap:.5rem;margin-bottom:.8rem}
h2{font-size:.68rem;font-weight:700;text-transform:uppercase;
  letter-spacing:.1em;color:var(--dim)}
.sw{position:relative;display:inline-flex;align-items:center;
  gap:.4rem;cursor:pointer;margin-left:auto}
.sw input{position:absolute;opacity:0;width:0;height:0}
.sw-t{width:40px;height:22px;background:#1a1a30;border-radius:11px;
  transition:background .22s;border:1px solid var(--bd);flex-shrink:0}
.sw input:checked~.sw-t{background:var(--acc)}
.sw-k{position:absolute;left:3px;width:16px;height:16px;background:#fff;
  border-radius:50%;transition:transform .22s;box-shadow:0 1px 3px rgba(0,0,0,.5)}
.sw input:checked~.sw-k{transform:translateX(18px)}
.sw-l{font-size:.72rem;color:var(--dim)}
.sliders{display:flex;flex-direction:column;gap:.85rem;margin-bottom:.85rem}
.sl-row{display:grid;grid-template-columns:5.5rem 1fr 3.2rem;align-items:center;gap:.5rem}
.sl-l{font-size:.75rem;color:var(--dim)}
.sl-v{font-size:.75rem;text-align:right;color:var(--txt);font-variant-numeric:tabular-nums}
input[type=range]{width:100%;height:5px;appearance:none;border-radius:3px;
  outline:none;cursor:pointer}
input[type=range]::-webkit-slider-thumb{appearance:none;width:15px;height:15px;
  border-radius:50%;background:#fff;box-shadow:0 1px 4px rgba(0,0,0,.7);
  transition:transform .1s}
input[type=range]:active::-webkit-slider-thumb{transform:scale(1.25)}
input[type=range]::-moz-range-thumb{width:15px;height:15px;border:none;
  border-radius:50%;background:#fff;box-shadow:0 1px 4px rgba(0,0,0,.7)}
#bsl{background:linear-gradient(90deg,#111 0%,#e8e8f0 100%)}
#tsl{background:linear-gradient(90deg,#f97316 0%,#a0c8ff 100%)}
#bsl::-moz-range-track{background:linear-gradient(90deg,#111,#e8e8f0);height:5px;border-radius:3px}
#tsl::-moz-range-track{background:linear-gradient(90deg,#f97316,#a0c8ff);height:5px;border-radius:3px}
.presets{display:grid;grid-template-columns:1fr 1fr;gap:.4rem;margin-bottom:.8rem}
.clr-row{display:grid;grid-template-columns:5.5rem 1fr 1.6rem;align-items:center;gap:.5rem;
  padding-top:.75rem;border-top:1px solid var(--bd)}
#hsl{background:linear-gradient(90deg,hsl(0,100%,50%),hsl(60,100%,50%),hsl(120,100%,50%),hsl(180,100%,50%),hsl(240,100%,50%),hsl(300,100%,50%),hsl(360,100%,50%))}
#hsl::-moz-range-track{background:linear-gradient(90deg,hsl(0,100%,50%),hsl(60,100%,50%),hsl(120,100%,50%),hsl(180,100%,50%),hsl(240,100%,50%),hsl(300,100%,50%),hsl(360,100%,50%));height:5px;border-radius:3px}
.hue-dot{width:18px;height:18px;border-radius:50%;border:1px solid rgba(255,255,255,.15);flex-shrink:0}
.pills{display:flex;flex-wrap:wrap;gap:.38rem;margin:.1rem 0 .5rem}
.pills button{padding:.32rem .7rem;border:1px solid var(--bd);border-radius:99px;
  background:var(--s2);color:var(--dim);font-size:.76rem;cursor:pointer;
  transition:all .18s;line-height:1}
.pills button.act{background:var(--acc);border-color:var(--acc2);color:#fff;
  box-shadow:0 0 14px rgba(139,92,246,.45)}
.prices{display:flex;flex-direction:column;gap:.5rem}
.pr{display:flex;align-items:baseline;gap:.4rem}
.pr-n{font-size:.72rem;color:var(--dim);width:3.8rem;flex-shrink:0}
.pr-v{font-size:.9rem;font-weight:600;font-variant-numeric:tabular-nums}
.pr-c{font-size:.72rem;margin-left:auto;font-variant-numeric:tabular-nums}
.up{color:var(--up)}.dn{color:var(--dn)}
.sys{display:grid;grid-template-columns:1fr 1fr;gap:.5rem .8rem;margin-bottom:.75rem}
.si{display:flex;align-items:center;justify-content:space-between;
  font-size:.8rem;background:var(--s2);border:1px solid var(--bd);
  border-radius:.6rem;padding:.45rem .7rem}
button{background:var(--s2);border:1px solid var(--bd);color:var(--txt);
  padding:.5rem .75rem;border-radius:.55rem;font-size:.8rem;cursor:pointer;
  transition:all .14s}
button:active{opacity:.72;transform:scale(.96)}
.media-btn{width:100%;padding:.7rem;font-size:.87rem;
  background:linear-gradient(135deg,#2a1a4a,#1a1a3a);
  border-color:rgba(139,92,246,.3)}
.media-btn:hover{border-color:var(--acc)}
</style>
</head>
<body>
<div class="hdr">
  <h1>Fin-ESP</h1>
  <span id="clk" class="clk"></span>
  <span class="wth" id="wth"></span>
  <span class="ft" id="ft"></span>
  <span class="dot" id="dot"></span>
</div>

<div class="card" id="lcard">
  <div class="ch">
    <h2>Lamp</h2>
    <label class="sw">
      <input type="checkbox" id="lpwr" onchange="if(!_upd)lampPow(this.checked)">
      <span class="sw-t"></span><span class="sw-k"></span>
    </label>
  </div>
  <div class="sliders">
    <div class="sl-row">
      <span class="sl-l">Brightness</span>
      <input type="range" id="bsl" min="0" max="100" value="50"
        oninput="slUpd()" onchange="sendLamp()">
      <span class="sl-v" id="bv">50%</span>
    </div>
    <div class="sl-row">
      <span class="sl-l">Warmth</span>
      <input type="range" id="tsl" min="0" max="100" value="50"
        oninput="slUpd()" onchange="sendLamp()">
      <span class="sl-v" id="tv">--</span>
    </div>
  </div>
  <div class="presets">
    <button onclick="preset('warm')">&#127775; Warm Dim</button>
    <button onclick="preset('bright')">&#9728; Bright White</button>
  </div>
  <div class="clr-row">
    <span class="sl-l">Colour</span>
    <input type="range" id="hsl" min="0" max="360" value="30"
      oninput="hueUpd()" onchange="sendHue()">
    <span class="hue-dot" id="hdot" style="background:hsl(30,100%,50%)"></span>
  </div>
</div>

<div class="card">
  <div class="ch">
    <h2>Screen</h2>
    <label class="sw">
      <input type="checkbox" id="ar" onchange="if(!_upd)setAR(this.checked)">
      <span class="sw-t"></span><span class="sw-k"></span>
      <span class="sw-l">auto-rotate</span>
    </label>
  </div>
  <div class="pills" id="spills">
    <button onclick="setScr(0)">BTC</button>
    <button onclick="setScr(1)">SOL</button>
    <button onclick="setScr(2)">Gold</button>
    <button onclick="setScr(3)">Oil</button>
    <button onclick="setScr(4)">USD/BRL</button>
  </div>
</div>

<div class="card">
  <div class="ch"><h2>Markets</h2></div>
  <div class="prices">
    <div class="pr"><span class="pr-n">BTC</span><span class="pr-v" id="pv0">-</span><span class="pr-c" id="pc0"></span></div>
    <div class="pr"><span class="pr-n">SOL</span><span class="pr-v" id="pv1">-</span><span class="pr-c" id="pc1"></span></div>
    <div class="pr"><span class="pr-n">Gold</span><span class="pr-v" id="pv2">-</span><span class="pr-c" id="pc2"></span></div>
    <div class="pr"><span class="pr-n">Oil</span><span class="pr-v" id="pv3">-</span><span class="pr-c" id="pc3"></span></div>
    <div class="pr"><span class="pr-n">USD/BRL</span><span class="pr-v" id="pv4">-</span><span class="pr-c" id="pc4"></span></div>
  </div>
</div>

<div class="card">
  <div class="ch"><h2>LEDs</h2></div>
  <div style="display:flex;flex-direction:column;gap:.45rem">
    <div style="display:flex;align-items:center;gap:.5rem">
      <span class="sl-l" style="width:3.8rem">Green</span>
      <div class="pills" style="margin:0">
        <button id="lg-0" onclick="ledSet('green',false)">Off</button>
        <button id="lg-1" onclick="ledSet('green',true)">On</button>
      </div>
    </div>
    <div style="display:flex;align-items:center;gap:.5rem">
      <span class="sl-l" style="width:3.8rem">Red</span>
      <div class="pills" style="margin:0">
        <button id="lr-0" onclick="ledSet('red',false)">Off</button>
        <button id="lr-1" onclick="ledSet('red',true)">On</button>
      </div>
    </div>
    <div style="display:flex;align-items:center;gap:.5rem">
      <span class="sl-l" style="width:3.8rem">Yellow</span>
      <div class="pills" style="margin:0">
        <button id="lb-0" onclick="ledSet('blue',false)">Off</button>
        <button id="lb-1" onclick="ledSet('blue',true)">On</button>
      </div>
    </div>
  </div>
</div>

<div class="card">
  <div class="ch"><h2>System</h2></div>
  <div class="sys">
    <div class="si">Display
      <label class="sw" style="margin-left:0">
        <input type="checkbox" id="disp" onchange="if(!_upd)act('/action/display/'+(this.checked?'on':'off'))">
        <span class="sw-t"></span><span class="sw-k"></span>
      </label>
    </div>
    <div class="si">Pot
      <label class="sw" style="margin-left:0">
        <input type="checkbox" id="pot" onchange="if(!_upd)act('/action/pot/'+(this.checked?'on':'off'))">
        <span class="sw-t"></span><span class="sw-k"></span>
      </label>
    </div>
  </div>
  <div class="sliders" style="margin-top:0;margin-bottom:.75rem">
    <div class="sl-row">
      <span class="sl-l">Volume</span>
      <input type="range" id="vsl" min="0" max="100" value="50"
        oninput="volUpd()" onmouseup="sendVol()" ontouchend="sendVol()">
      <span class="sl-v" id="vv">--</span>
    </div>
  </div>
  <button class="media-btn" onclick="act('/action/media')">&#9654;&#65039; Play / Pause</button>
  <button class="media-btn" style="margin-top:.5rem;border-color:rgba(239,68,68,.3);background:linear-gradient(135deg,#2a1010,#1a1010)" onclick="if(confirm('Reboot ESP32?'))act('/action/reboot')">&#x1F504; Reboot</button>
</div>

<div class="card">
  <div class="ch"><h2>Per-Machine Media</h2></div>
  <div style="display:flex;flex-direction:column;gap:.9rem">
    <div>
      <div class="sl-l" style="margin-bottom:.4rem">Desktop</div>
      <div class="sliders" style="margin-bottom:.5rem">
        <div class="sl-row">
          <span class="sl-l">Volume</span>
          <input type="range" id="vsl-desktop" min="0" max="100" value="50"
            oninput="volUpdM('desktop')" onmouseup="sendVolM('desktop')" ontouchend="sendVolM('desktop')">
          <span class="sl-v" id="vv-desktop">--</span>
        </div>
      </div>
      <button class="media-btn" onclick="actTarget('desktop')">&#9654;&#65039; Play / Pause (Desktop)</button>
    </div>
    <div>
      <div class="sl-l" style="margin-bottom:.4rem">Laptop</div>
      <div class="sliders" style="margin-bottom:.5rem">
        <div class="sl-row">
          <span class="sl-l">Volume</span>
          <input type="range" id="vsl-laptop" min="0" max="100" value="50"
            oninput="volUpdM('laptop')" onmouseup="sendVolM('laptop')" ontouchend="sendVolM('laptop')">
          <span class="sl-v" id="vv-laptop">--</span>
        </div>
      </div>
      <button class="media-btn" onclick="actTarget('laptop')">&#9654;&#65039; Play / Pause (Laptop)</button>
    </div>
  </div>
</div>

<script>
// Real hostnames the two machines identify themselves with (see
// mic_key_daemon.py/play_pause_server.py's MACHINE_ID = socket.gethostname()).
var MACHINE_IDS={desktop:'silent-ms7e56',laptop:'silent'};
function actTarget(who){
  post('/action/media/target?machine='+encodeURIComponent(MACHINE_IDS[who]));
}
function volUpdM(who){
  document.getElementById('vv-'+who).textContent=document.getElementById('vsl-'+who).value+'%';
}
function sendVolM(who){
  var v=+document.getElementById('vsl-'+who).value;
  post('/action/volume/target?machine='+encodeURIComponent(MACHINE_IDS[who])+'&v='+v);
}
var _upd=false;
function post(u){return fetch(u,{method:'POST',headers:{'X-FinESP':'1'}});}
function setChk(id,v){_upd=true;document.getElementById(id).checked=v;_upd=false;}
var lt=null;
var WN={0:'Clear',1:'Sunny',2:'PtCloud',3:'Overcast',45:'Fog',48:'IceFog',
  51:'Drizzle',53:'Drizzle',55:'H.Drzl',61:'Rain',63:'Rain',65:'H.Rain',
  71:'Snow',73:'Snow',75:'H.Snow',77:'IceGr',80:'Shwrs',81:'Shwrs',82:'H.Shwrs',
  85:'SnwShwr',86:'SnwShwr',95:'Storm',96:'Hail',99:'Hail'};
function wname(c){return WN[c]||'?'}
function tickClock(){
  var n=new Date();
  var h=n.getHours().toString().padStart(2,'0');
  var m=n.getMinutes().toString().padStart(2,'0');
  var s=n.getSeconds().toString().padStart(2,'0');
  document.getElementById('clk').textContent=h+':'+m+':'+s;
}
setInterval(tickClock,1000);tickClock();
function slUpd(){
  var b=+document.getElementById('bsl').value;
  var t=+document.getElementById('tsl').value;
  document.getElementById('bv').textContent=b+'%';
  document.getElementById('tv').textContent=
    t<15?'warm':t<40?'warm-ish':t<60?'neutral':t<85?'cool-ish':'cool';
}
var _serverVol=-1;
function volUpd(){
  document.getElementById('vv').textContent=document.getElementById('vsl').value+'%';
}
function sendVol(){
  var v=+document.getElementById('vsl').value;
  if(v===_serverVol)return;
  post('/action/volume?v='+v);
}
function hueUpd(){
  var h=+document.getElementById('hsl').value;
  document.getElementById('hdot').style.background='hsl('+h+',100%,50%)';
}
function sendHue(){
  if(_upd)return;
  var h=+document.getElementById('hsl').value;
  var bv=Math.round(+document.getElementById('bsl').value*10);
  post('/action/lamp/colour?h='+h+'&s=1000&v='+bv).then(refresh);
}
function sendLamp(){
  if(_upd)return;
  var b=document.getElementById('bsl').value;
  var t=document.getElementById('tsl').value;
  clearTimeout(lt);
  lt=setTimeout(function(){
    post('/action/lamp/set?brightness='+b+'&temp='+t);
  },150);
}
function lampPow(on){
  post('/action/lamp/'+(on?'on':'off')).then(refresh);
}
function preset(n){
  post('/action/lamp/'+n).then(refresh);
}
function setScr(i){
  post('/action/screen/set?s='+i).then(refresh);
}
function setAR(on){
  post('/action/autorotate/'+(on?'on':'off'));
}
function ledSet(c,on){post('/action/led/'+c+'/'+(on?'on':'off')).then(refresh);}
function ledUpd(c,on){
  var p=c[0];
  document.getElementById('l'+p+'-0').className=on?'':'act';
  document.getElementById('l'+p+'-1').className=on?'act':'';
}
function act(u){post(u).then(refresh);}
function fmtP(v,d){
  if(!v)return'-';
  return v.toLocaleString('en-US',{minimumFractionDigits:d,maximumFractionDigits:d});
}
function fmtC(c){
  if(c===null||c===undefined||c===0)return'';
  return'<span class="'+(c>0?'up':'dn')+'">'+(c>0?'+':'')+c.toFixed(2)+'%</span>';
}
function refresh(){
  fetch('/status').then(function(r){return r.json()}).then(function(d){
    document.getElementById('dot').className='dot'+(d.wifi?' ok':'');
    document.getElementById('ft').textContent=d.fetching?'...':'';
    if(d.weather_temp!==null&&d.weather_code!==null){
      document.getElementById('wth').textContent=
        wname(d.weather_code)+' '+d.weather_temp.toFixed(1)+'\u00B0C';
    }
    setChk('lpwr',d.lamp_on);
    document.getElementById('lcard').className='card'+(d.lamp_on?' glow':'');
    _upd=true;
    if(d.lamp_brightness!==null){document.getElementById('bsl').value=d.lamp_brightness;}
    if(d.lamp_temp!==null){document.getElementById('tsl').value=d.lamp_temp;}
    _upd=false;
    slUpd();
    setChk('ar',d.auto_rotate);
    var pills=document.getElementById('spills').querySelectorAll('button');
    for(var i=0;i<pills.length;i++)pills[i].className=i===d.screen_idx?'act':'';
    ledUpd('green',d.led_green);
    ledUpd('red',d.led_red);
    ledUpd('blue',d.led_blue);
    setChk('disp',d.display_on);
    setChk('pot',d.pot_on);
    if(d.volume!==null&&document.activeElement!==document.getElementById('vsl')){
      _serverVol=d.volume;
      document.getElementById('vsl').value=d.volume;
      document.getElementById('vv').textContent=d.volume+'%';
    }
    var p=d.prices;
    document.getElementById('pv0').textContent='$'+fmtP(p.btc,0);
    document.getElementById('pc0').innerHTML=fmtC(p.btc_chg);
    document.getElementById('pv1').textContent='$'+fmtP(p.sol,2);
    document.getElementById('pc1').innerHTML=fmtC(p.sol_chg);
    document.getElementById('pv2').textContent='$'+fmtP(p.gold,2);
    document.getElementById('pc2').innerHTML=fmtC(p.gold_chg);
    document.getElementById('pv3').textContent='$'+fmtP(p.oil,2);
    document.getElementById('pc3').innerHTML=fmtC(p.oil_chg);
    document.getElementById('pv4').textContent=fmtP(p.usd_brl,4);
    document.getElementById('pc4').innerHTML=fmtC(p.usd_brl_chg);
  }).catch(function(){document.getElementById('dot').className='dot';});
}
slUpd();refresh();setInterval(refresh,3000);
</script>
</body>
</html>"##;

// ── HTTP helpers ──────────────────────────────────────────────────────────────

fn write_response(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let hdr = std::format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status, ctype, body.len()
    );
    let _ = s.write_all(hdr.as_bytes());
    let _ = s.write_all(body);
}

fn ok(s: &mut TcpStream) {
    write_response(s, "200 OK", "text/plain", b"ok");
}

fn drain_headers(r: &mut BufReader<TcpStream>) -> bool {
    let mut from_app = false;
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return from_app,
            Ok(_) if line.trim_end().is_empty() => return from_app,
            _ => {
                if line.trim_end().to_ascii_lowercase() == "x-finesp: 1" {
                    from_app = true;
                }
            }
        }
    }
}

fn get_param(query: &str, key: &str) -> Option<i32> {
    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k == key { return v.parse().ok(); }
        }
    }
    None
}

fn get_param_str<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k == key && !v.is_empty() { return Some(v); }
        }
    }
    None
}

// ── Request handler ───────────────────────────────────────────────────────────

fn handle(
    stream: TcpStream,
    triggers: &WebTriggers,
    ui_state: &Arc<Mutex<UiState>>,
    screen_forced_off: &Arc<AtomicBool>,
    lamp: &Arc<LampHandle>,
    auto_rotate: &Arc<AtomicBool>,
    leds: &Arc<LedState>,
) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(stream);

    let mut first_line = String::new();
    if reader.read_line(&mut first_line).is_err() { return; }
    let from_app = drain_headers(&mut reader);

    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 { return; }

    let (path, query) = parts[1].split_once('?').unwrap_or((parts[1], ""));
    let method = parts[0];
    let mut s = reader.into_inner();
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());


    match (method, path) {
        ("GET", "/") => {
            write_response(&mut s, "200 OK", "text/html; charset=utf-8", HTML);
        }
        ("GET", "/status") => {
            let json = build_status(ui_state, screen_forced_off, lamp, auto_rotate, leds);
            write_response(&mut s, "200 OK", "application/json", json.as_bytes());
        }
        // ── Lamp ────────────────────────────────────────────────────────────
        ("POST", "/action/lamp/on") => {
            lamp.queue_on();
            if let Ok(mut st) = ui_state.lock() { st.lamp.on = true; st.lamp.known = true; }
            ok(&mut s);
        }
        ("POST", "/action/lamp/off") => {
            lamp.queue_off();
            if let Ok(mut st) = ui_state.lock() { st.lamp.on = false; st.lamp.known = true; }
            ok(&mut s);
        }
        ("POST", "/action/lamp/toggle") | ("POST", "/action/lamp") => {
            let current_on = ui_state.lock().map(|st| st.lamp.on).unwrap_or(false);
            let new_on = lamp.flip_target(current_on);
            if let Ok(mut st) = ui_state.lock() { st.lamp.on = new_on; st.lamp.known = true; }
            ok(&mut s);
        }
        ("POST", "/action/lamp/warm") => { lamp.queue_warm_dim(); ok(&mut s); }
        ("POST", "/action/lamp/bright") => { lamp.queue_bright_white(); ok(&mut s); }
        ("POST", "/action/lamp/set") => {
            let b_pct = get_param(query, "brightness").unwrap_or(50).clamp(0, 100) as u16;
            let t_pct = get_param(query, "temp").unwrap_or(50).clamp(0, 100) as u16;
            // 0-100% → Tuya brightness 10-1000, temp 0-1000
            let tuya_b = (b_pct as u32 * 990 / 100 + 10).clamp(10, 1000) as u16;
            let tuya_t = (t_pct as u32 * 10).clamp(0, 1000) as u16;
            lamp.queue_brightness_temp(tuya_b, tuya_t);
            ok(&mut s);
        }
        // ── Screen ──────────────────────────────────────────────────────────
        ("POST", "/action/screen") | ("POST", "/action/screen/next") => {
            triggers.screen.store(true, Ordering::Relaxed);
            ok(&mut s);
        }
        ("POST", "/action/screen/set") => {
            let idx = get_param(query, "s").unwrap_or(-1).clamp(-1, 4) as i8;
            triggers.screen_select.store(idx, Ordering::Relaxed);
            ok(&mut s);
        }
        // ── Auto-rotate ─────────────────────────────────────────────────────
        ("POST", "/action/autorotate/on")  => { auto_rotate.store(true,  Ordering::Relaxed); ok(&mut s); }
        ("POST", "/action/autorotate/off") => { auto_rotate.store(false, Ordering::Relaxed); ok(&mut s); }
        // ── System ──────────────────────────────────────────────────────────
        ("POST", "/action/display/on")  => {
            screen_forced_off.store(false, Ordering::Relaxed);
            triggers.display.store(1, Ordering::Relaxed);
            ok(&mut s);
        }
        ("POST", "/action/display/off") => {
            screen_forced_off.store(true, Ordering::Relaxed);
            triggers.display.store(0, Ordering::Relaxed);
            ok(&mut s);
        }
        ("POST", "/action/pot/on") => {
            if let Ok(mut st) = ui_state.lock() { st.pot_enabled = true; }
            triggers.pot.store(1, Ordering::Relaxed);
            ok(&mut s);
        }
        ("POST", "/action/pot/off") => {
            if let Ok(mut st) = ui_state.lock() { st.pot_enabled = false; }
            triggers.pot.store(0, Ordering::Relaxed);
            ok(&mut s);
        }
        ("POST", "/action/media")       => { triggers.media.store(true, Ordering::Relaxed); ok(&mut s); }
        ("POST", "/action/volume") => {
            let v = get_param(query, "v").unwrap_or(-1).clamp(0, 100);
            if v >= 0 {
                // slider 0-100 → VOLUME_PCT 0-153 (matching pot's sqrt curve max)
                let vol_raw = (v as u32 * 153 / 100) as u8;
                if let Ok(mut st) = ui_state.lock() { st.volume_pct = vol_raw; }
                triggers.volume.store(v as i8, Ordering::Relaxed);
            }
            ok(&mut s);
        }
        // ── Per-machine explicit control ───────────────────────────────────
        // Targets one named machine directly (see main.rs's MEDIA_TARGETS /
        // VOLUME_TARGETS + handle_media_connection) - independent of
        // CURRENT_OWNER, so either machine is controllable from the web
        // regardless of which one last toggled its mic.
        ("POST", "/action/media/target") => {
            if let Some(machine) = get_param_str(query, "machine") {
                crate::queue_media_for_machine(machine);
            }
            ok(&mut s);
        }
        ("POST", "/action/volume/target") => {
            let v = get_param(query, "v").unwrap_or(-1).clamp(0, 100);
            if let (Some(machine), true) = (get_param_str(query, "machine"), v >= 0) {
                let vol_raw = (v as u32 * 153 / 100) as u8;
                crate::queue_volume_for_machine(machine, vol_raw);
            }
            ok(&mut s);
        }
        ("POST", "/action/lamp/colour") => {
            let hue = get_param(query, "h").unwrap_or(30).clamp(0, 360) as u16;
            let sat = get_param(query, "s").unwrap_or(1000).clamp(0, 1000) as u16;
            let val = get_param(query, "v").unwrap_or(500).clamp(0, 1000) as u16;
            lamp.queue_colour(hue, sat, val);
            if let Ok(mut st) = ui_state.lock() { st.lamp.on = true; st.lamp.known = true; }
            ok(&mut s);
        }
        // ── LEDs ────────────────────────────────────────────────────────────
        ("POST", "/action/led/green/on")  => { leds.set_green(true);  ok(&mut s); }
        ("POST", "/action/led/green/off") => { leds.set_green(false); ok(&mut s); }
        ("POST", "/action/led/red/on")    => { leds.set_red(true);    ok(&mut s); }
        ("POST", "/action/led/red/off")   => { leds.set_red(false);   ok(&mut s); }
        ("POST", "/action/led/blue/on")   => { leds.set_blue(true);   ok(&mut s); }
        ("POST", "/action/led/blue/off")  => { leds.set_blue(false);  ok(&mut s); }
        ("POST", "/action/reboot") => {
            ok(&mut s);
            std::thread::sleep(Duration::from_millis(100));
            unsafe { esp_idf_sys::esp_restart(); }
        }
        _ => { write_response(&mut s, "404 Not Found", "text/plain", b"not found"); }
    }
}

fn build_status(
    ui_state: &Arc<Mutex<UiState>>,
    screen_forced_off: &Arc<AtomicBool>,
    lamp: &Arc<LampHandle>,
    auto_rotate: &Arc<AtomicBool>,
    leds: &Arc<LedState>,
) -> String {
    let st = ui_state.lock().unwrap();
    let sfo = screen_forced_off.load(Ordering::Relaxed);
    let ar  = auto_rotate.load(Ordering::Relaxed);
    let lb  = lamp.brightness_pct();
    let lt  = lamp.temp_pct();
    let d   = &st.data;

    let lb_json  = if lb == LAMP_UNKNOWN { "null".into() } else { lb.to_string() };
    let lt_json  = if lt == LAMP_UNKNOWN { "null".into() } else { lt.to_string() };
    let vol_json = if st.volume_pct == 255 { "null".into() } else { (st.volume_pct as u32 * 100 / 153).to_string() };
    let wt_json  = d.weather_temp.map(|t| std::format!("{:.1}", t))
        .unwrap_or_else(|| "null".into());
    let wc_json  = d.weather_code.map(|c| c.to_string())
        .unwrap_or_else(|| "null".into());

    std::format!(
        concat!(
            r#"{{"screen":"{scr}","screen_idx":{si},"lamp_on":{lon},"lamp_known":{lk},"#,
            r#""lamp_brightness":{lb},"lamp_temp":{lt},"display_on":{don},"pot_on":{pot},"#,
            r#""volume":{vol},"fetching":{fet},"wifi":{wifi},"auto_rotate":{ar},"#,
            r#""weather_temp":{wt},"weather_code":{wc},"#,
            r#""led_green":{lg},"led_red":{lr},"led_blue":{lbl},"#,
            r#""prices":{{"btc":{btc},"btc_chg":{bc},"sol":{sol},"sol_chg":{sc},"#,
            r#""gold":{gold},"gold_chg":{gc},"oil":{oil},"oil_chg":{oc},"#,
            r#""usd_brl":{brl},"usd_brl_chg":{brc}}}}}"#
        ),
        scr  = st.screen.name().trim(),
        si   = st.screen as u8,
        lon  = st.lamp.on,
        lk   = st.lamp.known,
        lb   = lb_json,
        lt   = lt_json,
        don  = !sfo,
        pot  = st.pot_enabled,
        vol  = vol_json,
        fet  = st.fetching,
        wifi = st.wifi_connected,
        ar   = ar,
        wt   = wt_json,
        wc   = wc_json,
        lg   = leds.green.load(Ordering::Relaxed),
        lr   = leds.red  .load(Ordering::Relaxed),
        lbl  = leds.blue .load(Ordering::Relaxed),
        btc  = d.price_btc,
        bc   = d.chg_btc_pct,
        sol  = d.price_sol,
        sc   = d.chg_sol_pct,
        gold = d.price_gold,
        gc   = d.chg_gold_pct,
        oil  = d.price_oil,
        oc   = d.chg_oil_pct,
        brl  = d.price_usd_brl,
        brc  = d.chg_usd_brl_pct,
    )
}

// ── Server loop ───────────────────────────────────────────────────────────────

pub fn spawn(
    triggers: Arc<WebTriggers>,
    ui_state: Arc<Mutex<UiState>>,
    screen_forced_off: Arc<AtomicBool>,
    lamp: Arc<LampHandle>,
    auto_rotate: Arc<AtomicBool>,
    leds: Arc<LedState>,
) {
    std::thread::Builder::new()
        .name("web-srv".into())
        .stack_size(10240)
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
                        Ok(s) => handle(s, &triggers, &ui_state, &screen_forced_off, &lamp, &auto_rotate, &leds),
                        Err(e) => { warn!("[web] accept err: {}", e); break; }
                    }
                }
            }
        })
        .ok();
}
