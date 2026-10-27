//! Fin-ESP as the LG AC's local cloud (replaces the desktop rethink). The AC connects to Fin-ESP
//! over WiFi; Fin-ESP serves the ThinQ /route endpoints, runs a 1-client MQTTS broker, speaks the
//! clip protocol, and decodes the TLV stream — so ALL the real two-way AC data lives on this one
//! board, no desktop. See docs/AC_CLOUD_PLAN.md for the full protocol spec.
//!
//! Disabled by default (AC_CLOUD_ENABLED). Enable only once each stage is proven, because this adds
//! a TLS handshake (~20-30 KB transient) to a board with a documented OOM-crash history — the heap
//! guards in web.rs and an added guard around the handshake must gate it.
//!
//! Build stages (docs/AC_CLOUD_PLAN.md):
//!   0. HTTPS /route + /route/certificate + /device/:id/certificate (present rethink's CA so the AC
//!      connects WITHOUT re-pairing). TEST: controlled /route re-advertise switch from desktop->Fin-ESP.
//!   1. MQTTS broker (1 client): CONNECT/CONNACK, SUBSCRIBE/SUBACK, PUBLISH both ways, PINGREQ.
//!   2. clip: deploy/preDeploy -> complete; device_packet -> TLV decode; req_timesync -> reply.
//!   3. decode type-ids (reuse ac.rs TLV) -> real state -> dashboard Live row reads FROM Fin-ESP.
//!   4. control via device_packet TLV; then AdGuard points the AC's cloud name here, retire rethink.

use log::info;

/// The CA the AC already trusts (copied from rethink's data/ca.cert). Presenting a cert chaining to
/// this lets the AC connect to Fin-ESP with no re-pairing. The private key is injected at build.
pub const AC_CA_CERT: &str = include_str!("../certs/ac_ca.cert");

/// Entry point — spawned from main only when config::AC_CLOUD_ENABLED. Currently a no-op stub while
/// the server is built stage by stage (see module docs).
pub fn spawn() {
    info!("[ac_cloud] stub — AC-cloud-on-Fin-ESP not yet implemented (see docs/AC_CLOUD_PLAN.md)");
    // Stage 0 next: bind an esp-idf HTTPS server on :443 serving /route + /route/certificate,
    // present a cert chaining to AC_CA_CERT. Then the MQTTS broker.
}
