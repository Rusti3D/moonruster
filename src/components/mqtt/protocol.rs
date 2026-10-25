// Reason codes, packet encoding, TLS setup

use super::*;

pub(super) fn connack_string(rc: u8) -> String {
    match rc {
        0 => "Connection Accepted.",
        1 => "Connection Refused: unacceptable protocol version.",
        2 => "Connection Refused: identifier rejected.",
        3 => "Connection Refused: broker unavailable.",
        4 => "Connection Refused: bad user name or password.",
        5 => "Connection Refused: not authorised.",
        _ => "Connection Refused: unknown reason.",
    }
    .into()
}

/// paho ReasonCode.getName() for the packet types we receive
pub(super) fn reason_name(rc: u8) -> String {
    match rc {
        0 => "Success",
        1 => "Granted QoS 1",
        2 => "Granted QoS 2",
        0x80 => "Unspecified error",
        0x81 => "Malformed packet",
        0x82 => "Protocol error",
        0x83 => "Implementation specific error",
        0x84 => "Unsupported protocol version",
        0x85 => "Client identifier not valid",
        0x86 => "Bad user name or password",
        0x87 => "Not authorized",
        0x88 => "Server unavailable",
        0x89 => "Server busy",
        0x8A => "Banned",
        0x8C => "Bad authentication method",
        0x8F => "Topic filter invalid",
        0x90 => "Topic name invalid",
        0x91 => "Packet identifier in use",
        0x95 => "Packet too large",
        0x97 => "Quota exceeded",
        0x99 => "Payload format invalid",
        0x9A => "Retain not supported",
        0x9B => "QoS not supported",
        0x9C => "Use another server",
        0x9D => "Server moved",
        0x9E => "Shared subscriptions not supported",
        0x9F => "Connection rate exceeded",
        0xA1 => "Subscription identifiers not supported",
        0xA2 => "Wildcard subscriptions not supported",
        _ => "Unknown",
    }
    .into()
}

// ***** Packet encoding *****

pub(super) fn put_str(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(&(s.len() as u16).to_be_bytes());
    buf.extend_from_slice(s);
}

pub(super) fn packet(header: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![header];
    let mut len = body.len();
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            b |= 0x80;
        }
        out.push(b);
        if len == 0 {
            break;
        }
    }
    out.extend(body);
    out
}

pub(super) fn read_varint(data: &[u8], pos: &mut usize) -> Option<usize> {
    let mut mult = 1usize;
    let mut val = 0usize;
    for _ in 0..4 {
        let b = *data.get(*pos)?;
        *pos += 1;
        val += (b & 0x7F) as usize * mult;
        if b & 0x80 == 0 {
            return Some(val);
        }
        mult *= 128;
    }
    None
}

/// Splits one complete packet off the front of the buffer
pub(super) fn take_packet(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    let mut pos = 1;
    let len = read_varint(buf, &mut pos)?;
    if buf.len() < pos + len {
        return None;
    }
    let header = buf[0];
    let body = buf[pos..pos + len].to_vec();
    buf.drain(..pos + len);
    Some((header, body))
}

pub(super) fn base62_id() -> String {
    pub(super) const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    (0..22).map(|_| CHARS[rand::random::<usize>() % 62] as char).collect()
}

pub(super) fn load_tls_config() -> Result<tokio_rustls::rustls::ClientConfig, String> {
    // paho's tls_set() uses the system CA store (OpenSSL default paths,
    // honoring SSL_CERT_FILE / SSL_CERT_DIR)
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    let certs = rustls_native_certs::load_native_certs();
    for cert in certs.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err("No system CA certificates found".into());
    }
    Ok(tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
}
