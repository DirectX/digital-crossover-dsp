use base64::prelude::*;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::thread;
use tokio_util::sync::CancellationToken;

use crate::config::*;
use crate::pipe::poll_readable;

pub fn spawn_thread(token: CancellationToken) {
    thread::spawn(move || {
        while !token.is_cancelled() {
            if !std::path::Path::new(METADATA_PATH).exists() {
                thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }

            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(METADATA_PATH)
            {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Failed to open metadata pipe: {}", e);
                    thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            let fd = file.as_raw_fd();
            let mut reader = BufReader::new(file);
            let mut buffer = String::new();

            loop {
                if token.is_cancelled() {
                    return;
                }
                if !poll_readable(fd, POLL_TIMEOUT_MS) {
                    continue;
                }
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        break;
                    }
                    Ok(_) => {
                        buffer.push_str(&line);
                        while let Some(end_idx) = buffer.find("<​/item>") {
                            let item_len = end_idx + 7;
                            let item_str = &buffer[..item_len];

                            process_metadata_item(item_str);

                            // Remove processed item from buffer
                            buffer = buffer[item_len..].to_string();
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(e) => {
                        eprintln!("Metadata reader error: {}", e);
                        break;
                    }
                }
            }

            // If we break out of the inner loop, wait and try to reconnect
            thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

fn process_metadata_item(item: &str) {
    let type_hex = extract_tag_content(item, "type");
    let code_hex = extract_tag_content(item, "code");
    let data_b64 = extract_data_content(item);

    if let (Some(t_hex), Some(c_hex), Some(b64)) = (type_hex, code_hex, data_b64) {
        let t = hex_to_ascii(&t_hex);
        let c = hex_to_ascii(&c_hex);

        // Clean up base64 whitespace for proper decoding
        let b64_clean = b64.replace("\n", "").replace("\r", "").replace(" ", "");

        if t == "core" {
            if let Ok(decoded_bytes) = BASE64_STANDARD.decode(&b64_clean) {
                let val = String::from_utf8_lossy(&decoded_bytes).trim().to_string();
                if !val.is_empty() {
                    match c.as_str() {
                        "minm" => println!("🎵 Track:  {}", val),
                        "asar" => println!("🎤 Artist: {}", val),
                        "asal" => println!("💿 Album:  {}", val),
                        _ => {} // Ignore other core tags
                    }
                }
            }
        } else if t == "ssnc" {
            match c.as_str() {
                "pbeg" => println!("▶️ Playback started"),
                "pend" => println!("⏸ Playback stopped"),
                "pfls" => println!("🔁 Stream flushed (Seek/Skip)"),
                _ => {} // Ignore volume/artwork tags for now
            }
        }
    }
}

// Minimal helpers to avoid bringing in a massive XML parsing crate
fn extract_tag_content(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<​{}>", tag);
    let close = format!("<​/{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].trim().to_string())
}

fn extract_data_content(xml: &str) -> Option<String> {
    let start = xml.find("<data")?;
    let close_bracket = xml[start..].find('>')?;
    let data_start = start + close_bracket + 1;
    let end_idx = xml[data_start..].find("<​/data>")?;
    Some(xml[data_start..data_start + end_idx].trim().to_string())
}

fn hex_to_ascii(hex: &str) -> String {
    let mut ascii = String::new();
    for i in 0..(hex.len() / 2) {
        if let Ok(b) = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
            ascii.push(b as char);
        }
    }
    ascii
}
