//! Minimal TLS *server* session over an accepted TcpStream, straight on esp-tls's C API.
//!
//! Why not esp-idf-svc's `EspTls::negotiate_server`: it is behind `#[cfg(esp_idf_esp_tls_server)]`,
//! which derives from a `CONFIG_ESP_TLS_SERVER` Kconfig option that no longer exists in ESP-IDF 5.4
//! (the server API is always built there) -- so on this toolchain the safe wrapper is compiled out.
//! The underlying `esp_tls_server_session_create` is in the generated bindings, so we call it.
//!
//! Every failure is a returned error; nothing here panics or aborts.

use esp_idf_sys as sys;
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::Duration;

/// Leaf certificate (CN/SAN rethink.internal, common.lgthinq.com, fin-esp.internal, IP 192.168.1.240)
/// signed by rethink's CA -- the CA the real AC already pinned at pairing, so it trusts Fin-ESP with
/// no re-pairing. PEM buffers must include the trailing NUL for mbedTLS.
static SERVER_CERT: &str = concat!(include_str!("../../certs/ac_server.cert"), "\0");
/// Its private key. Local-only file (gitignored); generated 2026-10-27, see certs/.
static SERVER_KEY: &str = concat!(include_str!("../../certs/ac_server.key"), "\0");

pub const WANT_READ: isize = sys::ESP_TLS_ERR_SSL_WANT_READ as isize;
pub const WANT_WRITE: isize = sys::ESP_TLS_ERR_SSL_WANT_WRITE as isize;

pub fn heap_free() -> u32 { unsafe { sys::esp_get_free_heap_size() } }
pub fn heap_largest() -> u32 { unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_8BIT) as u32 } }

pub struct TlsConn {
    raw: *mut sys::esp_tls_t,
    // Owns the fd. esp_tls_server_session_delete() does NOT close it, so dropping this closes it
    // exactly once (field drops run after our Drop::drop body).
    stream: TcpStream,
}

impl TlsConn {
    /// Run the server handshake on an accepted connection. Blocking, bounded by `timeout`
    /// (SO_RCVTIMEO/SO_SNDTIMEO: a stalled peer makes mbedTLS's recv fail -> handshake error).
    pub fn accept(stream: TcpStream, timeout: Duration) -> Result<Self, i32> {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));
        let _ = stream.set_nodelay(true);

        let raw = unsafe { sys::esp_tls_init() };
        if raw.is_null() {
            return Err(-1);
        }
        let mut cfg: sys::esp_tls_cfg_server_t = unsafe { core::mem::zeroed() };
        cfg.__bindgen_anon_3.servercert_buf = SERVER_CERT.as_ptr();
        cfg.__bindgen_anon_4.servercert_bytes = SERVER_CERT.len() as _;
        cfg.__bindgen_anon_5.serverkey_buf = SERVER_KEY.as_ptr();
        cfg.__bindgen_anon_6.serverkey_bytes = SERVER_KEY.len() as _;

        let ret = unsafe { sys::esp_tls_server_session_create(&mut cfg, stream.as_raw_fd(), raw) };
        if ret != 0 {
            // Frees whatever the failed create left behind (ssl/conf/cert/key/drbg) + the struct.
            unsafe { sys::esp_tls_server_session_delete(raw) };
            return Err(ret);
        }
        Ok(Self { raw, stream })
    }

    pub fn fd(&self) -> i32 { self.stream.as_raw_fd() }

    pub fn set_read_timeout(&self, d: Duration) { let _ = self.stream.set_read_timeout(Some(d)); }

    /// Decrypted bytes already buffered inside mbedTLS (poll() on the fd can't see these).
    pub fn pending(&self) -> bool { unsafe { sys::esp_tls_get_bytes_avail(self.raw) > 0 } }

    /// Wait up to `ms` for the socket to become readable (or for buffered plaintext).
    pub fn wait_readable(&self, ms: i32) -> bool {
        if self.pending() {
            return true;
        }
        poll_in(&[self.fd()], ms).map(|r| r[0]).unwrap_or(false)
    }

    /// One read. Ok(n>0) data, Ok(0) nothing yet (want-read), Err = closed or failed.
    /// Call only after wait_readable() (else it blocks up to the read timeout).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, isize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let r = unsafe { sys::esp_tls_conn_read(self.raw, buf.as_mut_ptr() as *mut _, buf.len()) };
        if r > 0 {
            Ok((r as usize).min(buf.len()))
        } else if r == WANT_READ || r == WANT_WRITE {
            Ok(0)
        } else {
            Err(if r == 0 { -0x7880 /* closed */ } else { r })
        }
    }

    pub fn write_all(&mut self, mut buf: &[u8]) -> Result<(), isize> {
        let mut stalls = 0u32;
        while !buf.is_empty() {
            let r = unsafe { sys::esp_tls_conn_write(self.raw, buf.as_ptr() as *const _, buf.len()) };
            if r > 0 {
                let n = (r as usize).min(buf.len());
                buf = &buf[n..];
                stalls = 0;
            } else if r == 0 || r == WANT_READ || r == WANT_WRITE {
                stalls += 1;
                if stalls > 100 {
                    return Err(r);
                }
                std::thread::sleep(Duration::from_millis(20));
            } else {
                return Err(r);
            }
        }
        Ok(())
    }
}

impl Drop for TlsConn {
    fn drop(&mut self) {
        unsafe { sys::esp_tls_server_session_delete(self.raw) };
        // `stream` drops after this -> close(fd).
    }
}

/// poll() for POLLIN on up to 4 fds. Returns per-fd readable/hangup flags, None on poll error.
pub fn poll_in(fds: &[i32], ms: i32) -> Option<[bool; 4]> {
    let mut p = [sys::pollfd { fd: -1, events: 0, revents: 0 }; 4];
    let n = fds.len().min(4);
    for i in 0..n {
        p[i].fd = fds[i];
        p[i].events = sys::POLLIN as _;
    }
    let r = unsafe { sys::poll(p.as_mut_ptr(), n as _, ms) };
    if r < 0 {
        return None;
    }
    let mut out = [false; 4];
    for i in 0..n {
        out[i] = p[i].revents as u32 & (sys::POLLIN | sys::POLLHUP | sys::POLLERR) != 0;
    }
    Some(out)
}
