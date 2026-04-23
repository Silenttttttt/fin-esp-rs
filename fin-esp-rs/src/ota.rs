/// Network OTA update server (TCP port 3232).
///
/// Protocol: client sends 4-byte big-endian length, then the raw ESP32
/// app binary (produced by `esptool.py elf2image`).
/// Server responds with "OK: rebooting\n" or "ERR: <reason>\n".
///
/// Flash with: ./flash_net.sh <ESP32_IP>

use log::info;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn handle_client(mut stream: TcpStream) {
    stream.set_read_timeout(Some(Duration::from_secs(120))).ok();

    // 4-byte big-endian image length
    let mut len_buf = [0u8; 4];
    if let Err(e) = stream.read_exact(&mut len_buf) {
        info!("[ota] failed to read length: {:?}", e);
        let _ = stream.write_all(b"ERR: read length\n");
        return;
    }
    let expected = u32::from_be_bytes(len_buf) as usize;
    info!("[ota] expecting {} bytes ({} KB)", expected, expected / 1024);

    // Next OTA partition
    let partition = unsafe {
        esp_idf_sys::esp_ota_get_next_update_partition(core::ptr::null())
    };
    if partition.is_null() {
        info!("[ota] no OTA partition found");
        let _ = stream.write_all(b"ERR: no OTA partition\n");
        return;
    }

    // Begin OTA
    let mut handle: esp_idf_sys::esp_ota_handle_t = 0;
    let ret = unsafe { esp_idf_sys::esp_ota_begin(partition, expected, &mut handle) };
    if ret != 0 {
        info!("[ota] esp_ota_begin failed: {}", ret);
        let _ = stream.write_all(b"ERR: ota begin\n");
        return;
    }

    // Receive + write in 4 KB chunks
    let mut buf = vec![0u8; 4096];
    let mut written = 0usize;
    let mut failed = false;

    while written < expected {
        let want = buf.len().min(expected - written);
        match stream.read(&mut buf[..want]) {
            Ok(0) => {
                info!("[ota] connection closed early at {} bytes", written);
                failed = true;
                break;
            }
            Ok(n) => {
                let ret = unsafe {
                    esp_idf_sys::esp_ota_write(handle, buf.as_ptr() as *const _, n)
                };
                if ret != 0 {
                    info!("[ota] esp_ota_write failed at {} bytes: {}", written, ret);
                    failed = true;
                    break;
                }
                written += n;
                if written % 65536 == 0 {
                    info!("[ota] {}KB / {}KB", written / 1024, expected / 1024);
                }
            }
            Err(e) => {
                info!("[ota] read error at {} bytes: {:?}", written, e);
                failed = true;
                break;
            }
        }
    }

    if failed {
        unsafe { esp_idf_sys::esp_ota_abort(handle) };
        let _ = stream.write_all(b"ERR: write failed\n");
        return;
    }

    // Finalize
    let ret = unsafe { esp_idf_sys::esp_ota_end(handle) };
    if ret != 0 {
        info!("[ota] esp_ota_end failed: {}", ret);
        let _ = stream.write_all(b"ERR: ota end\n");
        return;
    }

    let ret = unsafe { esp_idf_sys::esp_ota_set_boot_partition(partition) };
    if ret != 0 {
        info!("[ota] set_boot_partition failed: {}", ret);
        let _ = stream.write_all(b"ERR: set boot partition\n");
        return;
    }

    info!("[ota] SUCCESS — {} bytes written, rebooting", written);
    let _ = stream.write_all(b"OK: rebooting\n");
    // Flush TCP, then stop WiFi before restart so IRAM is clean for phy_init.
    std::thread::sleep(Duration::from_millis(500));
    unsafe {
        esp_idf_sys::esp_wifi_stop();
        esp_idf_sys::esp_wifi_deinit();
        esp_idf_sys::esp_restart();
    }
}

pub fn spawn_ota_server() {
    std::thread::Builder::new()
        .name("ota".into())
        .stack_size(16384)
        .spawn(|| {
            let listener = TcpListener::bind("0.0.0.0:3232")
                .expect("[ota] TcpListener::bind failed");
            info!("[ota] listening on :3232");
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let peer = s.peer_addr()
                            .map(|a| a.to_string())
                            .unwrap_or_else(|_| "?".into());
                        info!("[ota] connection from {}", peer);
                        handle_client(s);
                    }
                    Err(e) => {
                        info!("[ota] accept error: {:?}", e);
                    }
                }
            }
        })
        .unwrap();
}
