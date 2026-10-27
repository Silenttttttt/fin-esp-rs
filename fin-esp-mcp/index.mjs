#!/usr/bin/env node
/**
 * fin-esp-mcp -- Fin-ESP's own MCP server (stdio): the Tuya lamp, the three status LEDs and the
 * LG air conditioner, as tools any Claude Code session (or Jarvis) can call directly.
 *
 * Runs on the PC, not on the ESP32: Fin-ESP has ~8-15 KB of free heap and a documented history of
 * allocation-failure crashes in its web handler, so the firmware keeps its small plain-HTTP API
 * (fin-esp-rs/src/web.rs) and this process is the MCP layer on top of it.
 *
 * Device quirk: every POST needs a body (even empty) or Content-Length is missing and the request
 * hangs.
 *
 * Register with: claude mcp add -s user fin-esp -- node <this-file-absolute-path>
 * Device address: FIN_ESP_URL (default http://192.168.1.240).
 */

import { z } from "zod";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";

const BASE_URL = process.env.FIN_ESP_URL ?? "http://192.168.1.240";
const TIMEOUT_MS = 8000;

async function post(path, params) {
  const url = new URL(path, BASE_URL);
  if (params) for (const [k, v] of Object.entries(params)) url.searchParams.set(k, String(v));
  const res = await fetch(url, { method: "POST", body: "", signal: AbortSignal.timeout(TIMEOUT_MS) });
  if (!res.ok) throw new Error(`Fin-ESP ${path} failed: ${res.status} ${await res.text()}`);
  return res.text();
}

async function get(path) {
  const res = await fetch(new URL(path, BASE_URL), { signal: AbortSignal.timeout(TIMEOUT_MS) });
  if (!res.ok) throw new Error(`Fin-ESP ${path} failed: ${res.status} ${await res.text()}`);
  return res.text();
}

const text = (t) => ({ content: [{ type: "text", text: t }] });
const server = new McpServer({ name: "fin-esp", version: "1.0.0" });
const readOnly = { readOnlyHint: true };

// ── Status ──────────────────────────────────────────────────────────────────

server.registerTool(
  "fin_esp_status",
  {
    title: "Fin-ESP status",
    description:
      "Fin-ESP's full status: lamp on/off and white-mode brightness/warmth, status LEDs, WiFi, weather, and the " +
      "air conditioner (`ac` object). Lamp brightness/warmth are null in colour mode (expected, not a failure), " +
      "and reflect the last value the LAMP confirmed, which can lag a change by ~15-20 s.",
    inputSchema: {},
    annotations: readOnly,
  },
  async () => text(await get("/status")),
);

// ── Lamp (Tuya, over WiFi) ──────────────────────────────────────────────────

// Lamp tool names match the fin-esp server that lived inside Jarvis (finEspTools.ts), so this
// server is a drop-in replacement for it.
for (const [state, desc] of [
  ["on", "Turn the lamp on. The right tool for a plain 'turn on the light' with no colour/brightness/warmth named."],
  ["off", "Turn the lamp off. The right tool for a plain 'turn off the light'."],
  ["toggle", "Toggle the lamp on/off."],
]) {
  server.registerTool(
    `fin_esp_lamp_${state}`,
    { title: `Lamp ${state}`, description: desc, inputSchema: {} },
    async () => text(await post(`/action/lamp/${state}`)),
  );
}

server.registerTool(
  "fin_esp_lamp_warm",
  {
    title: "Lamp warm dim",
    description: "Dim warm-WHITE preset (1% brightness, warmest, no colour). For a named colour use fin_esp_lamp_colour.",
    inputSchema: {},
  },
  async () => text(await post("/action/lamp/warm")),
);

server.registerTool(
  "fin_esp_lamp_bright",
  {
    title: "Lamp bright white",
    description: "Full-brightness cool-WHITE preset (no colour).",
    inputSchema: {},
  },
  async () => text(await post("/action/lamp/bright")),
);

server.registerTool(
  "fin_esp_lamp_set",
  {
    title: "Lamp white brightness/warmth",
    description:
      "Plain WHITE light with a brightness and/or warmth; either may be given alone and the other is kept. Not for " +
      "named colours (use fin_esp_lamp_colour). The lamp applies changes over its own link and can take ~15-20 s " +
      "to report the new value in status; a non-error response already means Fin-ESP accepted it.",
    inputSchema: {
      brightness: z.number().min(0).max(100).optional().describe("0-100 percent."),
      temp: z.number().min(0).max(100).optional().describe("Warmth 0-100, NOT Kelvin: 0 warmest, 100 coolest."),
    },
  },
  async ({ brightness, temp }) => {
    // The firmware defaults an omitted brightness/temp to 50, so fill the missing side from the
    // lamp's current value (same fix Jarvis's tool carried, 2026-10-07 incident).
    const params = {};
    if (brightness !== undefined) params.brightness = brightness;
    if (temp !== undefined) params.temp = temp;
    if (brightness === undefined || temp === undefined) {
      try {
        const st = JSON.parse(await get("/status"));
        if (brightness === undefined && st.lamp_brightness !== null) params.brightness = st.lamp_brightness;
        if (temp === undefined && st.lamp_temp !== null) params.temp = st.lamp_temp;
      } catch {
        // best effort: the firmware default applies to the unspecified side
      }
    }
    return text(await post("/action/lamp/set", params));
  },
);

server.registerTool(
  "fin_esp_lamp_colour",
  {
    title: "Lamp colour",
    description:
      "Named colour via HSV (h 0 red, 60 yellow, 120 green, 180 cyan, 240 blue, 300 magenta). s and v are percent " +
      "here and converted to the device's 0-1000 Tuya units. After this, status shows brightness/warmth as null " +
      "(expected in colour mode).",
    inputSchema: {
      h: z.number().min(0).max(360),
      s: z.number().min(0).max(100).describe("Saturation percent; 100 = vivid."),
      v: z.number().min(0).max(100).describe("Brightness percent."),
    },
  },
  async ({ h, s, v }) => text(await post("/action/lamp/colour", { h, s: Math.round(s * 10), v: Math.round(v * 10) })),
);

server.registerTool(
  "fin_esp_led",
  {
    title: "Status LED",
    description: "One of Fin-ESP's three tiny indicator LEDs (not the room lamp).",
    inputSchema: { color: z.enum(["red", "green", "yellow"]), state: z.enum(["on", "off"]) },
  },
  async ({ color, state }) => text(await post(`/action/led/${color}/${state}`)),
);

// ── LG air conditioner (IR, fin-esp-rs/src/ac.rs) ───────────────────────────
// IR is one-way. Fin-ESP is the AC's ONLY controller: every frame carries the full setting,
// timers run on Fin-ESP itself and toggle codes are never sent, so its saved state IS the AC's
// state. A non-error response means Fin-ESP accepted the change and transmits it within ~0.3 s.

server.registerTool(
  "fin_esp_ac_status",
  {
    title: "AC status",
    description:
      "The air conditioner's state as Fin-ESP holds it (authoritative, Fin-ESP is its only controller): on, mode, " +
      "temp (C), fan, swing_v (up/down flap), swing_h (left/right), jet, off_in_min (sleep timer, null = none), " +
      "auto_clean, energy_saving, power_limit (%).",
    inputSchema: {},
    annotations: readOnly,
  },
  async () => text(await get("/ac/state")),
);

server.registerTool(
  "fin_esp_ac_power",
  {
    title: "AC on/off",
    description: "Turn the AC on (with its saved mode/temp/fan) or off. Turning it off cancels any sleep timer.",
    inputSchema: { on: z.boolean() },
  },
  async ({ on }) => text(await post(`/action/ac/${on ? "on" : "off"}`)),
);

server.registerTool(
  "fin_esp_ac_set",
  {
    title: "AC mode/temperature/fan",
    description:
      "Change mode, target temperature and/or fan; omitted fields keep their value. Always turns the AC on. " +
      "mode: cool, dry (dehumidify), fan (air only), auto (LG 'AI': the AC chooses), heat.",
    inputSchema: {
      mode: z.enum(["cool", "dry", "fan", "auto", "heat"]).optional(),
      temp: z.number().int().min(16).max(30).optional().describe("Celsius, 16-30."),
      fan: z.enum(["auto", "lowest", "low", "medium", "high", "max"]).optional(),
    },
  },
  async ({ mode, temp, fan }) => {
    const params = {};
    if (mode !== undefined) params.mode = mode;
    if (temp !== undefined) params.temp = temp;
    if (fan !== undefined) params.fan = fan;
    return text(await post("/action/ac/set", params));
  },
);

server.registerTool(
  "fin_esp_ac_preset",
  {
    title: "AC preset",
    description:
      "comfort = cool 21C auto fan (same as Fin-ESP's blue button); jet = LG Jet, maximum cooling; night = cool " +
      "24C quietest fan; dry = dehumidify 23C.",
    inputSchema: { name: z.enum(["comfort", "jet", "night", "dry"]) },
  },
  async ({ name }) => text(await post("/action/ac/preset", { name })),
);

server.registerTool(
  "fin_esp_ac_sleep",
  {
    title: "AC off timer",
    description: "Turn the AC off after N minutes (Fin-ESP's own timer; it sends OFF itself). 0 cancels.",
    inputSchema: { minutes: z.number().int().min(0).max(1440) },
  },
  async ({ minutes }) => text(await post("/action/ac/sleep", { min: minutes })),
);

server.registerTool(
  "fin_esp_ac_airflow",
  {
    title: "AC airflow direction",
    description:
      "vertical = up/down flap: swing (sweeps), a fixed position, or off (stops). horizontal = left/right: auto " +
      "(sweeps) or off. Only transmitted while the AC is on.",
    inputSchema: {
      vertical: z.enum(["swing", "highest", "high", "upper-mid", "middle", "low", "lowest", "off"]).optional(),
      horizontal: z.enum(["auto", "off"]).optional(),
    },
  },
  async ({ vertical, horizontal }) => {
    const out = [];
    if (vertical !== undefined) out.push(await post("/action/ac/swing_v", { pos: vertical }));
    if (horizontal !== undefined) out.push(await post("/action/ac/swing_h", { pos: horizontal }));
    return text(out.join("\n") || "nothing to change");
  },
);

server.registerTool(
  "fin_esp_ac_features",
  {
    title: "AC settings: auto-clean, energy saving, power limit",
    description:
      "auto_clean: after switching off, the AC runs its fan to dry the coil (prevents musty smell; confirmed working " +
      "on this unit). energy_saving: LG eco mode. power_limit: LG 'Energy Ctrl', caps the compressor at 100/80/60/40% " +
      "of full power. Omitted fields are unchanged. Sent while the AC is on (queued until next power-on otherwise).",
    inputSchema: {
      auto_clean: z.boolean().optional(),
      energy_saving: z.boolean().optional(),
      power_limit: z.union([z.literal(100), z.literal(80), z.literal(60), z.literal(40)]).optional(),
    },
  },
  async ({ auto_clean, energy_saving, power_limit }) => {
    const params = {};
    if (auto_clean !== undefined) params.auto_clean = auto_clean ? "on" : "off";
    if (energy_saving !== undefined) params.energy_saving = energy_saving ? "on" : "off";
    if (power_limit !== undefined) params.power_limit = power_limit;
    return text(await post("/action/ac/feature", params));
  },
);

server.registerTool(
  "fin_esp_ac_show_power",
  {
    title: "AC: show power draw on its display",
    description:
      "Momentary: the AC shows its live power draw (kW) on its own front display for a few seconds. The reading is " +
      "only visible on the AC, it is not reported back to Fin-ESP.",
    inputSchema: {},
  },
  async () => text(await post("/action/ac/show_power")),
);

server.registerTool(
  "fin_esp_ac_resend",
  {
    title: "AC resend state",
    description: "Re-transmit the full saved state (settings + airflow) to re-sync the AC, e.g. after a power cut.",
    inputSchema: {},
  },
  async () => text(await post("/action/ac/resend")),
);

await server.connect(new StdioServerTransport());
