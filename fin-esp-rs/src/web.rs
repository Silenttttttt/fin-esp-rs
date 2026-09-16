use esp_idf_hal::delay::FreeRtos;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI8, Ordering}};
use std::time::Duration;
use log::{info, warn};
use crate::api;
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
    // Set on every "/" (dashboard page) load -- main.rs's `weatherFetch` thread polls this to
    // fetch weather on-demand instead of on a fixed background timer (see config.rs's
    // `WEATHER_MIN_REFRESH_MS`, which rate-limits how often a load can actually trigger a
    // fetch). Deliberately NOT set from `/status` -- that's polled continuously while the
    // dashboard tab is already open, which would defeat the whole point of "on-demand".
    pub weather:       AtomicBool,
    // Set right before EVERY request is dispatched (see classify_origin
    // call below), read by the main loop alongside whichever trigger flag
    // above a given request just set. Good enough for a personal, rarely-
    // concurrent device - two web requests landing within the same ~1ms
    // main-loop tick could momentarily pair the wrong origin with the
    // wrong trigger, but that's a non-issue at this traffic level.
    pub last_origin_external: AtomicBool,
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
            weather:       AtomicBool::new(false),
            last_origin_external: AtomicBool::new(false),
        }
    }
}

// The k3s node's own LAN IP (see config::URL_RABBITMQ_SENDER for the same
// address used elsewhere) - kube-proxy masquerades ALL NodePort-routed
// traffic to this address by default (standard kube-proxy behavior for an
// endpoint outside the pod network, which this "Service without selector"
// external device is), regardless of the true original client. A direct
// LAN client's own peer IP is essentially never this exact address, so
// peer == this address is the practical signal for "arrived via the
// NodePort (30881), i.e. the VPS over Tailscale" vs genuine direct-LAN
// access. The one false-positive case - someone browsing from the desktop
// itself rather than through it - is an accepted, rare edge case, not
// airtight security classification.
const K3S_NODE_IP: &str = "192.168.1.105";

fn classify_origin(peer: &str) -> &'static str {
    if peer.starts_with(K3S_NODE_IP) { "web-external" } else { "web-internal" }
}

// ── HTML ──────────────────────────────────────────────────────────────────────
const HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Fin-ESP</title>
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'%3E%3Crect width='100' height='100' rx='22' fill='%238b5cf6'/%3E%3Ctext x='50' y='71' font-size='58' text-anchor='middle'%3E%E2%9A%A1%3C/text%3E%3C/svg%3E">
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
.chart-legend{display:flex;align-items:center;gap:.3rem;margin-top:.7rem;font-size:.7rem;color:var(--dim)}
.chart-legend .lg-l{margin-right:.6rem}
.lg-dot{width:8px;height:8px;border-radius:50%;flex-shrink:0}
.climchart{width:100%;height:90px;margin-top:.4rem;display:block}
.chart-meta{font-size:.68rem;color:var(--dim);text-align:center;margin-top:.15rem;font-variant-numeric:tabular-nums}
.pulse-dot{animation:pulseDot 1.6s ease-in-out infinite}
@keyframes pulseDot{0%,100%{opacity:1}50%{opacity:.3}}
.climchart-wrap{animation:chartFade .35s ease}
@keyframes chartFade{from{opacity:.25}to{opacity:1}}
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
  <div class="ch"><h2>Climate</h2></div>
  <div class="prices">
    <div class="pr"><span class="pr-n">Temp</span><span class="pr-v" id="tempv">--</span></div>
    <div class="pr"><span class="pr-n">Humidity</span><span class="pr-v" id="humv">--</span></div>
  </div>
  <div class="chart-legend">
    <span class="lg-dot" style="background:#f97316"></span><span class="lg-l">Temp (&deg;C)</span>
    <span class="lg-dot" style="background:#38bdf8"></span><span class="lg-l">Humidity (%)</span>
  </div>
  <div class="climchart-wrap" id="climchartwrap">
    <svg id="climchart" viewBox="0 0 300 90" preserveAspectRatio="none" class="climchart"></svg>
  </div>
  <div class="pr-n" id="chartrange" style="text-align:center;margin-top:.3rem"></div>
  <div class="chart-meta" id="chartmeta"></div>
  <div class="pills" id="chartRangeBtns" style="justify-content:center;margin-top:.4rem">
    <button data-h="0" class="act" onclick="loadRange(0)">Live</button>
    <button data-h="1" onclick="loadRange(1)">1h</button>
    <button data-h="6" onclick="loadRange(6)">6h</button>
    <button data-h="24" onclick="loadRange(24)">24h</button>
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
        <button id="ly-0" onclick="ledSet('yellow',false)">Off</button>
        <button id="ly-1" onclick="ledSet('yellow',true)">On</button>
      </div>
    </div>
  </div>
</div>

<div class="card">
  <div class="ch"><h2>System</h2></div>
  <!-- Display/Pot toggles and the untargeted Play/Pause+Volume controls were
       removed from this card on purpose: the screen and potentiometer are
       physically disconnected (dead hardware - see config::DISPLAY_TOGGLE_ENABLED
       / config::POT_TOGGLE_ENABLED, both false, firmware support kept intact,
       just gated off), and untargeted media control is now fully superseded
       by the "Per-Machine Media" card below (explicit Desktop/Laptop control
       instead of ambiguous "whichever machine last toggled its mic"). The
       underlying /action/display, /action/pot, /action/media, /action/volume
       routes still exist server-side - only this UI's use of them was removed. -->
  <button class="media-btn" style="border-color:rgba(239,68,68,.3);background:linear-gradient(135deg,#2a1010,#1a1010)" onclick="if(confirm('Reboot ESP32?'))act('/action/reboot')">&#x1F504; Reboot</button>
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
function ledSet(c,on){post('/action/led/'+c+'/'+(on?'on':'off')).then(refresh);}
function ledUpd(c,on){
  var p=c[0];
  document.getElementById('l'+p+'-0').className=on?'':'act';
  document.getElementById('l'+p+'-1').className=on?'act':'';
}
function act(u){post(u).then(refresh);}
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
    ledUpd('green',d.led_green);
    ledUpd('red',d.led_red);
    ledUpd('yellow',d.led_yellow);
    document.getElementById('tempv').textContent=d.dht_temp!==null?d.dht_temp.toFixed(1)+'\u00B0C':'--';
    document.getElementById('humv').textContent=d.dht_humidity!==null?d.dht_humidity.toFixed(1)+'%':'--';
  }).catch(function(){document.getElementById('dot').className='dot';});
}
// Catmull-Rom-through-cubic-bezier smoothing -- a compact, well-known way to get a smooth
// TradingView-style curve through a point series without pulling in a charting library.
function smoothPath(xs,vals,mn,mx){
  var n=xs.length;
  function y(i){return 90-((vals[i]-mn)/(mx-mn))*90;}
  var d='M'+xs[0].toFixed(1)+','+y(0).toFixed(1)+' ';
  for(var i=0;i<n-1;i++){
    var i0=i>0?i-1:0, i2=i+1, i3=i+2<n?i+2:i+1;
    var cp1x=xs[i]+(xs[i2]-xs[i0])/6, cp1y=y(i)+(y(i2)-y(i0))/6;
    var cp2x=xs[i2]-(xs[i3]-xs[i])/6, cp2y=y(i2)-(y(i3)-y(i))/6;
    d+='C'+cp1x.toFixed(1)+','+cp1y.toFixed(1)+' '+cp2x.toFixed(1)+','+cp2y.toFixed(1)+' '+xs[i2].toFixed(1)+','+y(i2).toFixed(1)+' ';
  }
  return d;
}
var lastFetchAt=null;
function renderChart(events){
  var pts=events.slice().reverse();
  var wrap=document.getElementById('climchartwrap');
  var svg=document.getElementById('climchart');
  if(pts.length<2){svg.innerHTML='';document.getElementById('chartrange').textContent='not enough data yet';return;}
  var temps=pts.map(function(e){return e.metadata.temp_c});
  var hums=pts.map(function(e){return e.metadata.humidity_pct});
  var tMin=Math.min.apply(null,temps),tMax=Math.max.apply(null,temps);
  var hMin=Math.min.apply(null,hums),hMax=Math.max.apply(null,hums);
  if(tMin===tMax){tMin-=1;tMax+=1;}
  if(hMin===hMax){hMin-=1;hMax+=1;}
  var n=pts.length;
  var xs=[];
  for(var i=0;i<n;i++){xs.push((i/(n-1))*300);}
  var tPath=smoothPath(xs,temps,tMin,tMax);
  var hPath=smoothPath(xs,hums,hMin,hMax);
  var lastX=xs[n-1].toFixed(1);
  var lastTY=(90-((temps[n-1]-tMin)/(tMax-tMin))*90).toFixed(1);
  var lastHY=(90-((hums[n-1]-hMin)/(hMax-hMin))*90).toFixed(1);
  svg.innerHTML=
    '<defs>'+
    '<linearGradient id="gT" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#f97316" stop-opacity="0.32"/><stop offset="100%" stop-color="#f97316" stop-opacity="0"/></linearGradient>'+
    '<linearGradient id="gH" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#38bdf8" stop-opacity="0.32"/><stop offset="100%" stop-color="#38bdf8" stop-opacity="0"/></linearGradient>'+
    '</defs>'+
    '<path d="'+tPath+'L'+lastX+',90 L0,90 Z" fill="url(#gT)" stroke="none"/>'+
    '<path d="'+hPath+'L'+lastX+',90 L0,90 Z" fill="url(#gH)" stroke="none"/>'+
    '<path d="'+tPath+'" fill="none" stroke="#f97316" stroke-width="2" vector-effect="non-scaling-stroke"/>'+
    '<path d="'+hPath+'" fill="none" stroke="#38bdf8" stroke-width="2" vector-effect="non-scaling-stroke"/>'+
    '<text x="3" y="9" font-size="7" fill="#f97316" opacity="0.85">'+tMax.toFixed(1)+'</text>'+
    '<text x="3" y="87" font-size="7" fill="#f97316" opacity="0.85">'+tMin.toFixed(1)+'</text>'+
    '<text x="297" y="9" font-size="7" fill="#38bdf8" text-anchor="end" opacity="0.85">'+hMax.toFixed(1)+'</text>'+
    '<text x="297" y="87" font-size="7" fill="#38bdf8" text-anchor="end" opacity="0.85">'+hMin.toFixed(1)+'</text>'+
    '<circle cx="'+lastX+'" cy="'+lastTY+'" r="3" fill="#f97316" class="pulse-dot"/>'+
    '<circle cx="'+lastX+'" cy="'+lastHY+'" r="3" fill="#38bdf8" class="pulse-dot"/>';
  // Restart the fade-in animation on every redraw (a class toggle alone won't retrigger a
  // CSS animation already applied -- removing then re-adding after a reflow does) so each
  // refresh reads as a genuine live update, not a static swap.
  wrap.classList.remove('climchart-wrap');
  void wrap.offsetWidth;
  wrap.classList.add('climchart-wrap');
  function fmt(iso){var d=new Date(iso);return d.getHours().toString().padStart(2,'0')+':'+d.getMinutes().toString().padStart(2,'0');}
  document.getElementById('chartrange').textContent=
    fmt(pts[0].created_at)+' \u2192 '+fmt(pts[pts.length-1].created_at)+' ('+n+' readings)';
  lastFetchAt=Date.now();
}
var chartMode=0; // 0 = Live, else the preset hours
function tickChartMeta(){
  var el=document.getElementById('chartmeta');
  if(!lastFetchAt){el.textContent='';return;}
  var secAgo=Math.floor((Date.now()-lastFetchAt)/1000);
  var txt='updated '+secAgo+'s ago';
  // Past 30s with no successful re-render means the last few fetch attempts failed silently
  // (this device's own well-documented occasional WiFi blips) -- "next in 0s" forever would
  // read as broken/stuck, so say so plainly instead once we're clearly overdue.
  if(chartMode===0){
    txt+=secAgo>32?' \u00b7 retrying\u2026':' \u00b7 next in '+Math.max(0,30-secAgo)+'s';
  }
  el.textContent=txt;
}
setInterval(tickChartMeta,1000);
// Shared by the default "Live" fetch and the 1h/6h/24h buttons -- both now serve the same
// bucketed [epoch,temp,hum] point format (2026-08-31: Live moved off its own raw-event
// endpoint onto this one too, see dht22.rs's own comment for why), converted into the
// {created_at,metadata} shape renderChart already expects.
function pointsToEvents(points){
  return (points||[]).slice().reverse()
    .filter(function(p){return p[1]!==null&&p[2]!==null})
    .map(function(p){return {created_at:new Date(p[0]*1000).toISOString(),
      metadata:{temp_c:p[1],humidity_pct:p[2]}};});
}
function refreshChart(){
  chartMode=0;
  fetch('/dht/chart').then(function(r){return r.json()}).then(function(d){
    renderChart(pointsToEvents(d.points));
  }).catch(function(){});
}
var chartAutoTimer=setInterval(refreshChart,30000);
var rangePoll=null;
function setRangeActive(h){
  var btns=document.querySelectorAll('#chartRangeBtns button');
  for(var i=0;i<btns.length;i++){
    btns[i].className=(+btns[i].getAttribute('data-h')===h)?'act':'';
  }
}
function loadRange(h){
  setRangeActive(h);
  if(rangePoll){clearInterval(rangePoll);rangePoll=null;}
  if(h===0){
    if(!chartAutoTimer)chartAutoTimer=setInterval(refreshChart,30000);
    refreshChart();
    return;
  }
  if(chartAutoTimer){clearInterval(chartAutoTimer);chartAutoTimer=null;}
  chartMode=h;
  document.getElementById('chartrange').textContent='loading '+h+'h\u2026';
  post('/dht/chart/request?hours='+h);
  function poll(){
    fetch('/dht/chart/range?hours='+h).then(function(r){return r.json()}).then(function(d){
      if(d.pending)return;
      clearInterval(rangePoll);rangePoll=null;
      renderChart(pointsToEvents(d.points));
    }).catch(function(){});
  }
  rangePoll=setInterval(poll,1000);
  poll();
}
refreshChart();
slUpd();refresh();setInterval(refresh,3000);
</script>
</body>
</html>"##;

// ── HTTP helpers ──────────────────────────────────────────────────────────────

// Corrected same day, real regression found via live browser testing: the first version of
// this fix held `network_lock` for the ENTIRE `handle()` call, including this function's own
// multi-chunk retry loop (each retry sleeping 200ms) -- a slow page load could hold the lock
// for several real seconds, which starved every OTHER short consumer waiting on it, including
// `/status` (polled by the page's own JS every 3 seconds) and finFetch/Tuya themselves. That
// showed up live as `/status` failing repeatedly (ERR_EMPTY_RESPONSE/CONNECTION_RESET/
// CONNECTION_REFUSED) and the main page's own `ERR_CONTENT_LENGTH_MISMATCH` recurring on nearly
// every load -- worse than before this "fix", not better. Matches this file's own established
// granularity elsewhere (`exprotocol_task.rs` locks only for each short `pump()` call, sleeping
// OUTSIDE the lock between) -- now locking only around each individual `write_all` call here,
// released immediately after, with every sleep/retry-wait happening outside the lock.
fn write_header(s: &mut TcpStream, status: &str, ctype: &str, body_len: usize, network_lock: &Mutex<()>) -> bool {
    let hdr = std::format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status, ctype, body_len
    );
    let _g = network_lock.lock().unwrap();
    if let Err(e) = s.write_all(hdr.as_bytes()) {
        warn!("[web] header write failed ({} bytes): {e}", hdr.len());
        return false;
    }
    true
}

/// Single attempt per chunk, no retry, no sleep -- meant to be called while holding an
/// EXTERNAL lock (e.g. dht22's chart-cache mutex) that must not be held for long. Returns how
/// many bytes made it through before the first failure (== body.len() if everything succeeded).
/// A caller with bytes left over falls back to `write_body_retrying` with an OWNED copy of the
/// remainder, made only in that rare case, after releasing its own lock -- see this file's
/// `/dht/chart` route for why (2026-08-30: holding that cache's mutex through the OLD
/// full-retry write meant one slow/stalled client could block the chart-cache-refresher
/// thread, and this file's whole single accept-loop thread, for up to ~75-100s worst case).
fn write_body_fast(s: &mut TcpStream, body: &[u8], network_lock: &Mutex<()>) -> usize {
    let mut written = 0usize;
    while written < body.len() {
        let end = (written + 1024).min(body.len());
        let write_result = { let _g = network_lock.lock().unwrap(); s.write_all(&body[written..end]) };
        if write_result.is_err() {
            return written;
        }
        written = end;
    }
    written
}

// Real hardware data (2026-08-29): one large write_all(body) for the full ~16.7 KB page
// reliably got 0 bytes through; chunking into 1KB pieces with a per-chunk retry gets most or
// all of it through instead -- kept, just no longer holding network_lock across the sleeps.
fn write_body_retrying(s: &mut TcpStream, body: &[u8], network_lock: &Mutex<()>) {
    let mut written = 0usize;
    let mut failed = false;
    while written < body.len() {
        let end = (written + 1024).min(body.len());
        let mut attempt = 0;
        loop {
            let write_result = { let _g = network_lock.lock().unwrap(); s.write_all(&body[written..end]) };
            match write_result {
                Ok(()) => break,
                Err(e) if attempt < 3 => {
                    attempt += 1;
                    warn!("[web] chunk write stalled at {written}/{} bytes (attempt {attempt}): {e}, retrying", body.len());
                    FreeRtos::delay_ms(200);
                }
                Err(e) => {
                    warn!("[web] body write failed at {written}/{} bytes after retries: {e}", body.len());
                    failed = true;
                    break;
                }
            }
        }
        if failed { break; }
        written = end;
        FreeRtos::delay_ms(5);
    }
    if !failed {
        info!("[web] body write complete: {written} bytes");
    }
}

fn write_response(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8], network_lock: &Mutex<()>) {
    if !write_header(s, status, ctype, body.len(), network_lock) {
        return;
    }
    write_body_retrying(s, body, network_lock);
}

fn ok(s: &mut TcpStream, network_lock: &Mutex<()>) {
    write_response(s, "200 OK", "text/plain", b"ok", network_lock);
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
    network_lock: &Arc<Mutex<()>>,
) {
    // Root-caused live 2026-08-29: a request landing during `finFetch`'s multi-second HTTPS
    // burst (or lampBridge's own network-heavy section) had NO coordination with either --
    // unlike `exprotocol_task.rs`/`finFetch`/`lampBridge`, which all already join this same
    // lock specifically because their network-heavy sections must never overlap. Confirmed via
    // a real capture: a response header write (just 126 bytes) blocked for the full 5s write
    // timeout and failed with EAGAIN while `finFetch` was mid-burst ("[API] fetching gold"
    // logged seconds before) -- not a heap allocation failure, a genuine TX-path starvation
    // while WiFi bandwidth/buffers were tied up elsewhere. NOTE: locking is now done per-write
    // inside `write_response`/`ok`, NOT for this whole function -- an earlier version locked
    // here for the whole request and caused a real regression (see `write_response`'s own
    // comment for the live evidence).
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    // Found live 2026-08-29: `handle()` runs synchronously in `spawn()`'s own single
    // accept-loop thread -- there's no per-connection thread here at all (deliberately; that's
    // what avoids this file's own thread-spawn-storm bug class). But `write_response`'s
    // `s.write_all(body)` for the full HTML page (~16.7 KB) had NO write timeout, only the read
    // timeout above -- a client that stops reading (a stalled connection, flaky WiFi, or just a
    // browser tab given up on) leaves that write blocked indefinitely, which starves the ENTIRE
    // web server from accepting any other connection for as long as it's stuck. `/status`'s
    // small JSON body always completed fine; only the large page reliably hung. A write timeout
    // turns "blocks forever" into "this one client's write fails, move on" -- matching every
    // other timeout already in this codebase's own established pattern.
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    // `BufReader::new()` allocates Rust's default 8 KiB buffer -- on every single incoming web
    // request, unconditionally. Root-caused via a real hardware crash: GDB against a core dump
    // this device saved to flash (see project_esp32_heap_budget.md) resolved a clean, fully
    // symbolic `memory allocation ... failed -> abort()` backtrace bottoming EXACTLY in this
    // function, at the `Box<[u8]>::new_uninit_slice()` call BufReader::new's 8192-byte
    // allocation compiles down to. A single failed allocation here takes down the ENTIRE
    // device -- there's no Result to catch, `handle()` isn't in a position to recover from an
    // OOM this deep in std's own buffer setup. `drain_headers` below only ever needs to hold
    // ONE header line at a time (`read_line` refills incrementally), and every request this
    // endpoint actually serves is a short GET/POST control line plus a handful of small browser
    // headers -- 512 bytes is generous for that, and a 512-byte allocation is a dramatically
    // safer bet under heap pressure than 8192.
    // `BufReader::with_capacity` has no fallible path -- its internal allocation failing goes
    // straight to `handle_alloc_error` -> `abort()`, taking down the ENTIRE DEVICE (confirmed by
    // the real coredump this comment already references, and reconfirmed via a second real
    // coredump 2026-08-29 bottoming in the OTA handler's own equally-unguarded `vec![]`, the same
    // bug class). Checking free heap immediately before this exact allocation -- not as a
    // network_lock-style coordination gate (already tried and rejected for that, see
    // project_esp32_heap_budget.md: a point-in-time check is a poor predictor across a
    // multi-second window) -- is a tight, same-instant check with no meaningful time-of-check-to
    // time-of-use gap, so it's a legitimate guard here even though it wasn't there for that other
    // case. 8192 is a generous multiple of the 512 bytes actually needed, so this only ever
    // rejects a connection when the device is already in the danger zone this whole file's
    // history is about.
    // Corrected same day: 8192 was picked as "a generous multiple of 512" without checking
    // this device's real baseline -- turned out its normal free heap routinely sits at
    // 8-15 KiB even outside any fetch burst (confirmed live), so that threshold was rejecting
    // nearly every request, not just genuinely dangerous ones. 2048 is still 4x the actual
    // 512-byte need and comfortably above the smallest real allocation failure this project
    // has ever observed (208 bytes), without being so conservative it starves normal use.
    //
    // Corrected again (2026-08-30): this was the one remaining guard in the whole codebase
    // still checking TOTAL free heap (`esp_get_free_heap_size`) instead of the largest
    // CONTIGUOUS free block -- and it aborted the whole device for real, caught via a fresh
    // coredump, bottoming in this exact `BufReader::with_capacity` call. Every other guard in
    // this file/project was already corrected to `heap_caps_get_largest_free_block` after this
    // exact distinction (total-free-heap-looks-fine-but-too-fragmented) was proven out
    // elsewhere the same day -- this one just hadn't been swept yet.
    if unsafe { esp_idf_sys::heap_caps_get_largest_free_block(esp_idf_sys::MALLOC_CAP_8BIT) } < 2048 {
        warn!("[web] heap too fragmented to safely allocate reader buffer, rejecting connection");
        return;
    }
    let mut reader = BufReader::with_capacity(512, stream);

    let mut first_line = String::new();
    if reader.read_line(&mut first_line).is_err() { return; }
    let from_app = drain_headers(&mut reader);

    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 { return; }

    let (path, query) = parts[1].split_once('?').unwrap_or((parts[1], ""));
    let method = parts[0];
    let mut s = reader.into_inner();
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());
    triggers.last_origin_external.store(peer.starts_with(K3S_NODE_IP), Ordering::Relaxed);

    match (method, path) {
        ("GET", "/") => {
            triggers.weather.store(true, Ordering::Relaxed);
            write_response(&mut s, "200 OK", "text/html; charset=utf-8", HTML, network_lock);
        }
        ("GET", "/status") => {
            // Only ever polled while the dashboard page is actually open (every 3s, via its
            // own JS) -- lets dht22.rs's reader switch to a fast "live" sampling cadence for as
            // long as that keeps happening. O(1), no network I/O, safe to call unconditionally.
            crate::dht22::mark_dashboard_active();
            // Real coredump-confirmed whole-device abort (2026-08-30): `build_status`'s own
            // `std::format!()` call has no fallible allocation path (same bug class as this
            // whole file's `BufReader::with_capacity` guard above, and this session's
            // `serde_json::json!()` fixes in api.rs/dht22.rs) -- a growth failure inside
            // `alloc::fmt::format::format_inner` goes straight to `handle_alloc_error ->
            // abort()`. This route is polled every 3s by the page's own JS, making it one of
            // the most frequently-exercised allocation sites on the whole device -- same
            // same-instant free-heap guard as this file's other one, skip-and-retry instead of
            // gambling on it.
            //
            // Checks the largest CONTIGUOUS free block, not total free heap -- corrected
            // 2026-08-30 after a SECOND real coredump caught this exact route crashing again
            // even with the total-free-heap version of this guard already in place: total free
            // heap can look perfectly healthy while too fragmented for one ~1KB contiguous
            // allocation, this device's own well-documented recurring failure mode (see
            // project_esp32_heap_budget.md's fragmentation-ceiling investigation).
            if unsafe { esp_idf_sys::heap_caps_get_largest_free_block(esp_idf_sys::MALLOC_CAP_8BIT) } < 2048 {
                warn!("[web] heap too low to safely build /status, rejecting connection");
                return;
            }
            let json = build_status(ui_state, screen_forced_off, lamp, auto_rotate, leds);
            write_response(&mut s, "200 OK", "application/json", json.as_bytes(), network_lock);
        }
        // Same-origin relay for the Climate chart -- see config::URL_EVENT_DASHBOARD_HISTORY's
        // own comment for why this can't just be a direct cross-origin fetch from the browser.
        // Reads a CACHE only (see dht22::spawn_chart_cache_refresher) -- never makes the actual
        // outbound HTTP call here. That call used to happen directly in this handler, and a
        // real live bug this same session proved why that's wrong: `handle()` runs on this
        // file's one and only accept-loop thread, so a slow/unreachable dashboard blocked THIS
        // request AND every other request (including /status's 3s poll) for the full
        // timeout+retry window -- confirmed via a bare `curl` to this route hanging with zero
        // response. Reading a cache here is instant, no network I/O, no way to block anyone.
        ("GET", "/dht/chart") => {
            // No clone on the fast path -- see dht22::with_cached_chart_json's own comment for
            // why (a real whole-device abort, then a real "permanently empty chart" regression
            // from the heap-guard fix that was tried first). Only falls back to an owned copy
            // -- made AFTER releasing the cache lock -- if a slow/stalled client is actually
            // detected, so that rare case can't hold the lock for the old write-retry loop's
            // full worst-case duration (see write_body_fast's own comment: a real symptom this
            // caused, confirmed live: the whole device looking "hung" until power-cycled).
            let remainder = crate::dht22::with_cached_chart_json(|json| {
                let json = json.unwrap_or("{\"points\":[]}");
                let body = json.as_bytes();
                if !write_header(&mut s, "200 OK", "application/json", body.len(), network_lock) {
                    return None;
                }
                let written = write_body_fast(&mut s, body, network_lock);
                if written < body.len() { Some(body[written..].to_vec()) } else { None }
            });
            if let Some(remainder) = remainder {
                write_body_retrying(&mut s, &remainder, network_lock);
            }
        }
        // "Show more" 1h/6h/24h buttons -- POST just flags the range as wanted (never touches
        // the network itself, see dht22::request_range's own comment for why this is serviced
        // by the existing chart-cache-refresher thread rather than a new one), GET reads back
        // whatever's cached so far.
        ("POST", "/dht/chart/request") => {
            let hours = get_param(query, "hours").unwrap_or(0) as u32;
            if crate::dht22::request_range(hours) {
                ok(&mut s, network_lock);
            } else {
                write_response(&mut s, "400 Bad Request", "text/plain", b"invalid hours", network_lock);
            }
        }
        ("GET", "/dht/chart/range") => {
            let hours = get_param(query, "hours").unwrap_or(0) as u32;
            // Same fast-path/fallback split as /dht/chart above, same reason.
            let remainder = crate::dht22::with_cached_range_json(hours, |json| {
                let json = json.unwrap_or("{\"pending\":true}");
                let body = json.as_bytes();
                if !write_header(&mut s, "200 OK", "application/json", body.len(), network_lock) {
                    return None;
                }
                let written = write_body_fast(&mut s, body, network_lock);
                if written < body.len() { Some(body[written..].to_vec()) } else { None }
            });
            if let Some(remainder) = remainder {
                write_body_retrying(&mut s, &remainder, network_lock);
            }
        }
        // ── Lamp ────────────────────────────────────────────────────────────
        ("POST", "/action/lamp/on") => {
            lamp.queue_on();
            if let Ok(mut st) = ui_state.lock() { st.lamp.on = true; st.lamp.known = true; }
            api::report_event("button_press", "info", "lamp button (web)".to_string(), classify_origin(&peer));
            ok(&mut s, network_lock);
        }
        ("POST", "/action/lamp/off") => {
            lamp.queue_off();
            if let Ok(mut st) = ui_state.lock() { st.lamp.on = false; st.lamp.known = true; }
            api::report_event("button_press", "info", "lamp button (web)".to_string(), classify_origin(&peer));
            ok(&mut s, network_lock);
        }
        ("POST", "/action/lamp/toggle") | ("POST", "/action/lamp") => {
            let current_on = ui_state.lock().map(|st| st.lamp.on).unwrap_or(false);
            let new_on = lamp.flip_target(current_on);
            if let Ok(mut st) = ui_state.lock() { st.lamp.on = new_on; st.lamp.known = true; }
            api::report_event("button_press", "info", "lamp button (web)".to_string(), classify_origin(&peer));
            ok(&mut s, network_lock);
        }
        ("POST", "/action/lamp/warm") => {
            lamp.queue_warm_dim();
            api::report_event("button_press", "info", "warm dim button (web)".to_string(), classify_origin(&peer));
            ok(&mut s, network_lock);
        }
        ("POST", "/action/lamp/bright") => {
            lamp.queue_bright_white();
            api::report_event("button_press", "info", "bright white button (web)".to_string(), classify_origin(&peer));
            ok(&mut s, network_lock);
        }
        ("POST", "/action/lamp/set") => {
            let b_pct = get_param(query, "brightness").unwrap_or(50).clamp(0, 100) as u16;
            let t_pct = get_param(query, "temp").unwrap_or(50).clamp(0, 100) as u16;
            // 0-100% → Tuya brightness 10-1000, temp 0-1000
            let tuya_b = (b_pct as u32 * 990 / 100 + 10).clamp(10, 1000) as u16;
            let tuya_t = (t_pct as u32 * 10).clamp(0, 1000) as u16;
            lamp.queue_brightness_temp(tuya_b, tuya_t);
            ok(&mut s, network_lock);
        }
        // ── Screen ──────────────────────────────────────────────────────────
        ("POST", "/action/screen") | ("POST", "/action/screen/next") => {
            triggers.screen.store(true, Ordering::Relaxed);
            ok(&mut s, network_lock);
        }
        ("POST", "/action/screen/set") => {
            let idx = get_param(query, "s").unwrap_or(-1).clamp(-1, 4) as i8;
            triggers.screen_select.store(idx, Ordering::Relaxed);
            ok(&mut s, network_lock);
        }
        // ── Auto-rotate ─────────────────────────────────────────────────────
        ("POST", "/action/autorotate/on")  => { auto_rotate.store(true,  Ordering::Relaxed); ok(&mut s, network_lock); }
        ("POST", "/action/autorotate/off") => { auto_rotate.store(false, Ordering::Relaxed); ok(&mut s, network_lock); }
        // ── System ──────────────────────────────────────────────────────────
        ("POST", "/action/display/on")  => {
            screen_forced_off.store(false, Ordering::Relaxed);
            triggers.display.store(1, Ordering::Relaxed);
            ok(&mut s, network_lock);
        }
        ("POST", "/action/display/off") => {
            screen_forced_off.store(true, Ordering::Relaxed);
            triggers.display.store(0, Ordering::Relaxed);
            ok(&mut s, network_lock);
        }
        ("POST", "/action/pot/on") => {
            if let Ok(mut st) = ui_state.lock() { st.pot_enabled = true; }
            triggers.pot.store(1, Ordering::Relaxed);
            ok(&mut s, network_lock);
        }
        ("POST", "/action/pot/off") => {
            if let Ok(mut st) = ui_state.lock() { st.pot_enabled = false; }
            triggers.pot.store(0, Ordering::Relaxed);
            ok(&mut s, network_lock);
        }
        ("POST", "/action/media")       => { triggers.media.store(true, Ordering::Relaxed); ok(&mut s, network_lock); }
        ("POST", "/action/volume") => {
            let v = get_param(query, "v").unwrap_or(-1).clamp(0, 100);
            if v >= 0 {
                // slider 0-100 → VOLUME_PCT 0-153 (matching pot's sqrt curve max)
                let vol_raw = (v as u32 * 153 / 100) as u8;
                if let Ok(mut st) = ui_state.lock() { st.volume_pct = vol_raw; }
                triggers.volume.store(v as i8, Ordering::Relaxed);
            }
            ok(&mut s, network_lock);
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
            ok(&mut s, network_lock);
        }
        ("POST", "/action/volume/target") => {
            let v = get_param(query, "v").unwrap_or(-1).clamp(0, 100);
            if let (Some(machine), true) = (get_param_str(query, "machine"), v >= 0) {
                let vol_raw = (v as u32 * 153 / 100) as u8;
                crate::queue_volume_for_machine(machine, vol_raw);
            }
            ok(&mut s, network_lock);
        }
        ("POST", "/action/lamp/colour") => {
            let hue = get_param(query, "h").unwrap_or(30).clamp(0, 360) as u16;
            let sat = get_param(query, "s").unwrap_or(1000).clamp(0, 1000) as u16;
            let val = get_param(query, "v").unwrap_or(500).clamp(0, 1000) as u16;
            lamp.queue_colour(hue, sat, val);
            if let Ok(mut st) = ui_state.lock() { st.lamp.on = true; st.lamp.known = true; }
            ok(&mut s, network_lock);
        }
        // ── LEDs ────────────────────────────────────────────────────────────
        ("POST", "/action/led/green/on")  => { leds.set_green(true);  ok(&mut s, network_lock); }
        ("POST", "/action/led/green/off") => { leds.set_green(false); ok(&mut s, network_lock); }
        ("POST", "/action/led/red/on")    => { leds.set_red(true);    ok(&mut s, network_lock); }
        ("POST", "/action/led/red/off")   => { leds.set_red(false);   ok(&mut s, network_lock); }
        ("POST", "/action/led/yellow/on")   => { leds.set_yellow(true);   ok(&mut s, network_lock); }
        ("POST", "/action/led/yellow/off")  => { leds.set_yellow(false);  ok(&mut s, network_lock); }
        ("POST", "/action/reboot") => {
            ok(&mut s, network_lock);
            std::thread::sleep(Duration::from_millis(100));
            unsafe { esp_idf_sys::esp_restart(); }
        }
        _ => { write_response(&mut s, "404 Not Found", "text/plain", b"not found", network_lock); }
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
    let (dht_temp_json, dht_hum_json) = match crate::dht22::last_reading() {
        Some((t, h)) => (std::format!("{:.1}", t), std::format!("{:.1}", h)),
        None => ("null".into(), "null".into()),
    };

    std::format!(
        concat!(
            r#"{{"screen":"{scr}","screen_idx":{si},"lamp_on":{lon},"lamp_known":{lk},"#,
            r#""lamp_brightness":{lb},"lamp_temp":{lt},"display_on":{don},"pot_on":{pot},"#,
            r#""volume":{vol},"fetching":{fet},"wifi":{wifi},"auto_rotate":{ar},"#,
            r#""weather_temp":{wt},"weather_code":{wc},"#,
            r#""dht_temp":{dt},"dht_humidity":{dh},"#,
            r#""led_green":{lg},"led_red":{lr},"led_yellow":{lyl},"#,
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
        dt   = dht_temp_json,
        dh   = dht_hum_json,
        lg   = leds.green .load(Ordering::Relaxed),
        lr   = leds.red   .load(Ordering::Relaxed),
        lyl  = leds.yellow.load(Ordering::Relaxed),
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
    network_lock: Arc<Mutex<()>>,
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
                        Ok(s) => handle(s, &triggers, &ui_state, &screen_forced_off, &lamp, &auto_rotate, &leds, &network_lock),
                        Err(e) => { warn!("[web] accept err: {}", e); break; }
                    }
                }
            }
        })
        .ok();
}
