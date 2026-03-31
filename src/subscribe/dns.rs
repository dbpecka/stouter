use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::info;

// ---------------------------------------------------------------------------
// DNS name encoding / decoding
// ---------------------------------------------------------------------------

/// Parse a DNS wire-format name starting at `start` in `buf`.
///
/// Handles plain length-prefixed labels, the null terminator, and pointer
/// compression (RFC 1035 §4.1.4).
///
/// Returns `(name, next_offset)` where `next_offset` is the byte position
/// immediately after the name (or after the compression pointer, if any).
fn parse_dns_name(buf: &[u8], start: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut pos = start;
    let mut end_pos: Option<usize> = None;

    loop {
        if pos >= buf.len() {
            return None;
        }
        let byte = buf[pos];

        if byte == 0 {
            // Null terminator — name is complete.
            if end_pos.is_none() {
                end_pos = Some(pos + 1);
            }
            break;
        } else if byte & 0xC0 == 0xC0 {
            // Pointer compression.
            if pos + 1 >= buf.len() {
                return None;
            }
            if end_pos.is_none() {
                end_pos = Some(pos + 2);
            }
            let offset = (((byte & 0x3F) as usize) << 8) | buf[pos + 1] as usize;
            pos = offset;
        } else {
            // Plain label.
            let label_len = byte as usize;
            pos += 1;
            if pos + label_len > buf.len() {
                return None;
            }
            let label = std::str::from_utf8(&buf[pos..pos + label_len]).ok()?;
            labels.push(label.to_owned());
            pos += label_len;
        }
    }

    let name = labels.join(".");
    Some((name, end_pos.unwrap_or(pos + 1)))
}

/// Write a DNS wire-format name to `out`.
///
/// Each non-empty label is encoded as a length byte followed by label bytes.
/// A zero byte terminates the name.
fn write_dns_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.') {
        if label.is_empty() {
            continue;
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

// ---------------------------------------------------------------------------
// DNS response builders
// ---------------------------------------------------------------------------

/// Build a DNS A-record response with an SRV record in the additional section.
///
/// The additional SRV record conveys the port number the service listens on.
/// Returns the full DNS response message as a byte vector.
fn build_a_response(req_id: u16, req_rd: bool, name: &str, ip: [u8; 4], port: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);

    // Header
    out.extend_from_slice(&req_id.to_be_bytes()); // ID
    out.push(0x84 | (req_rd as u8)); // QR=1, Opcode=0, AA=1, TC=0, RD=copy
    out.push(0x00); // RA=0, Z=0, RCODE=0 (NOERROR)
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT = 1
    out.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT = 1
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT = 0
    out.extend_from_slice(&1u16.to_be_bytes()); // ARCOUNT = 1

    // Question
    write_dns_name(&mut out, name);
    out.extend_from_slice(&1u16.to_be_bytes()); // QTYPE  = A (1)
    out.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN (1)

    // Answer — A record
    write_dns_name(&mut out, name);
    out.extend_from_slice(&1u16.to_be_bytes()); // TYPE  = A (1)
    out.extend_from_slice(&1u16.to_be_bytes()); // CLASS = IN (1)
    out.extend_from_slice(&60u32.to_be_bytes()); // TTL = 60 s
    out.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH = 4
    out.extend_from_slice(&ip); // RDATA

    // Additional — SRV record (priority=0, weight=0, port, target=name)
    write_srv_record(&mut out, name, port);

    out
}

/// Build a DNS SRV-record response.
///
/// Returns the full DNS response message as a byte vector.
fn build_srv_response(req_id: u16, req_rd: bool, name: &str, port: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);

    // Header
    out.extend_from_slice(&req_id.to_be_bytes()); // ID
    out.push(0x84 | (req_rd as u8)); // QR=1, Opcode=0, AA=1, TC=0, RD=copy
    out.push(0x00); // RA=0, Z=0, RCODE=0 (NOERROR)
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT = 1
    out.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT = 1
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT = 0
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT = 0

    // Question
    write_dns_name(&mut out, name);
    out.extend_from_slice(&33u16.to_be_bytes()); // QTYPE  = SRV (33)
    out.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN (1)

    // Answer — SRV record
    write_srv_record(&mut out, name, port);

    out
}

/// Write an SRV resource record for `name` with `port` and target pointing to `name`.
///
/// Uses priority=0, weight=0, and a TTL of 60 seconds.
fn write_srv_record(out: &mut Vec<u8>, name: &str, port: u16) {
    write_dns_name(out, name);
    out.extend_from_slice(&33u16.to_be_bytes()); // TYPE  = SRV (33)
    out.extend_from_slice(&1u16.to_be_bytes()); // CLASS = IN (1)
    out.extend_from_slice(&60u32.to_be_bytes()); // TTL = 60 s

    // RDATA: priority(2) + weight(2) + port(2) + target(variable)
    let mut rdata = Vec::new();
    rdata.extend_from_slice(&0u16.to_be_bytes()); // Priority = 0
    rdata.extend_from_slice(&0u16.to_be_bytes()); // Weight   = 0
    rdata.extend_from_slice(&port.to_be_bytes()); // Port
    write_dns_name(&mut rdata, name); // Target

    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes()); // RDLENGTH
    out.extend_from_slice(&rdata);
}

/// Build a DNS NXDOMAIN response (no answer section).
fn build_nxdomain_response(req_id: u16, req_rd: bool, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);

    // Header
    out.extend_from_slice(&req_id.to_be_bytes()); // ID
    out.push(0x84 | (req_rd as u8)); // QR=1, Opcode=0, AA=1, TC=0, RD=copy
    out.push(0x03); // RA=0, Z=0, RCODE=3 (NXDOMAIN)
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT = 1
    out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT = 0
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT = 0
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT = 0

    // Question
    write_dns_name(&mut out, name);
    out.extend_from_slice(&1u16.to_be_bytes()); // QTYPE  = A (1)
    out.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN (1)

    out
}

// ---------------------------------------------------------------------------
// DNS server
// ---------------------------------------------------------------------------

/// Run a minimal UDP DNS server on `127.0.0.1:{port}`.
///
/// Responds to A-record queries for `<service>.stouter.local` by looking up
/// the service name in `service_ports` and returning `127.0.0.1` when found.
/// All other queries receive an NXDOMAIN response.
pub async fn run_dns(
    service_ports: Arc<RwLock<HashMap<String, u16>>>,
    port: u16,
) -> Result<()> {
    let socket = UdpSocket::bind(format!("127.0.0.1:{port}"))
        .await
        .with_context(|| format!("bind DNS socket on 127.0.0.1:{port}"))?;

    info!("DNS listening on 127.0.0.1:{port}");

    let mut buf = [0u8; 512];

    loop {
        let (n, src) = socket.recv_from(&mut buf).await.context("DNS recv_from")?;
        if n < 12 {
            continue; // Malformed — too short for a DNS header.
        }

        let req_id = u16::from_be_bytes([buf[0], buf[1]]);
        let req_rd = buf[2] & 0x01 != 0;

        let response = match parse_dns_name(&buf[..n], 12) {
            None => build_nxdomain_response(req_id, req_rd, ""),
            Some((name, name_end)) => {
                if name_end + 2 > n {
                    build_nxdomain_response(req_id, req_rd, &name)
                } else {
                    let qtype = u16::from_be_bytes([buf[name_end], buf[name_end + 1]]);

                    const STOUTER_SUFFIX: &str = ".stouter.local";
                    if (qtype == 1 || qtype == 33) && name.ends_with(STOUTER_SUFFIX) {
                        let service_name = &name[..name.len() - STOUTER_SUFFIX.len()];
                        let ports = service_ports.read().await;
                        if let Some(&svc_port) = ports.get(service_name) {
                            if qtype == 33 {
                                build_srv_response(req_id, req_rd, &name, svc_port)
                            } else {
                                build_a_response(req_id, req_rd, &name, [127, 0, 0, 1], svc_port)
                            }
                        } else {
                            build_nxdomain_response(req_id, req_rd, &name)
                        }
                    } else {
                        build_nxdomain_response(req_id, req_rd, &name)
                    }
                }
            }
        };

        let _ = socket.send_to(&response, src).await;
    }
}
