// PASSIVE: listens to rethink's device monitor ws, records raw frames (never sends to the AC).
// Usage: node capture.mjs <out.json> [seconds] [deviceId]
import WebSocket from "ws";
import fs from "fs";
const id = process.argv[4] || process.env.RETHINK_DEVICE_ID || "YOUR-DEVICE-UUID-HERE";
const out = [];
const ws = new WebSocket("ws://127.0.0.1:44401/device?id=" + id);
ws.on("message", (m) => { try { const j = JSON.parse(m); if (j.rx) out.push({ rx: j.rx }); else if (j.tx) out.push({ tx: j.tx }); } catch {} });
setTimeout(() => { fs.writeFileSync(process.argv[2], JSON.stringify(out, null, 1)); console.log("frames", out.length); process.exit(0); }, Number(process.argv[3] || 120) * 1000);
