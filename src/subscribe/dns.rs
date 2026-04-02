use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::info;

use super::ServiceInfo;

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
    // Guard against circular pointer compression references.
    let mut hops = 0u8;
    const MAX_HOPS: u8 = 64;

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
            hops += 1;
            if hops > MAX_HOPS {
                return None;
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

/// Look up a service by its `.stouter.local` name or by a custom domain.
///
/// Returns `Some(port)` if the queried name matches a known service.
fn lookup_service(services: &HashMap<String, ServiceInfo>, name: &str) -> Option<u16> {
    const STOUTER_SUFFIX: &str = ".stouter.local";

    // Try <service>.stouter.local first.
    if let Some(service_name) = name.strip_suffix(STOUTER_SUFFIX) {
        if let Some(info) = services.get(service_name) {
            return Some(info.port);
        }
    }

    // Try custom domains.
    for info in services.values() {
        if info.domains.iter().any(|d| d.eq_ignore_ascii_case(name)) {
            return Some(info.port);
        }
    }

    None
}

/// Run a minimal UDP DNS server on `127.0.0.1:{port}`.
///
/// Responds to A-record and SRV queries for `<service>.stouter.local` names
/// and custom domains by looking up the service in `service_ports` and
/// returning `127.0.0.1` when found. All other queries receive NXDOMAIN.
pub async fn run_dns(
    service_ports: Arc<RwLock<HashMap<String, ServiceInfo>>>,
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
            Some((raw_name, name_end)) => {
                // DNS names are case-insensitive (RFC 1035 §2.3.3).
                let name = raw_name.to_ascii_lowercase();
                if name_end + 2 > n {
                    build_nxdomain_response(req_id, req_rd, &name)
                } else {
                    let qtype = u16::from_be_bytes([buf[name_end], buf[name_end + 1]]);

                    if qtype == 1 || qtype == 33 {
                        let services = service_ports.read().await;
                        if let Some(svc_port) = lookup_service(&services, &name) {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_dns_name -------------------------------------------------------

    #[test]
    fn parse_simple_name() {
        // "example.com\0"
        let buf = b"\x07example\x03com\x00";
        let (name, end) = parse_dns_name(buf, 0).unwrap();
        assert_eq!(name, "example.com");
        assert_eq!(end, buf.len());
    }

    #[test]
    fn parse_name_with_offset() {
        // 4 bytes of padding, then "foo.bar\0"
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(b"\x03foo\x03bar\x00");
        let (name, end) = parse_dns_name(&buf, 4).unwrap();
        assert_eq!(name, "foo.bar");
        assert_eq!(end, buf.len());
    }

    #[test]
    fn parse_pointer_compression() {
        // offset 0: "example.com\0" (13 bytes)
        // offset 13: pointer to offset 0
        let mut buf = b"\x07example\x03com\x00".to_vec();
        buf.push(0xC0);
        buf.push(0x00);
        let (name, end) = parse_dns_name(&buf, 13).unwrap();
        assert_eq!(name, "example.com");
        assert_eq!(end, 15); // past the 2-byte pointer
    }

    #[test]
    fn parse_circular_pointer_returns_none() {
        // Two pointers that reference each other: offset 0 -> offset 2, offset 2 -> offset 0
        let buf = [0xC0, 0x02, 0xC0, 0x00];
        assert!(parse_dns_name(&buf, 0).is_none());
    }

    #[test]
    fn parse_truncated_label_returns_none() {
        // Claims label length 5 but only 2 bytes follow
        let buf = b"\x05ab";
        assert!(parse_dns_name(buf, 0).is_none());
    }

    #[test]
    fn parse_root_name() {
        let buf = [0x00]; // just null terminator
        let (name, end) = parse_dns_name(&buf, 0).unwrap();
        assert_eq!(name, "");
        assert_eq!(end, 1);
    }

    // -- write_dns_name -------------------------------------------------------

    #[test]
    fn write_simple_name() {
        let mut out = Vec::new();
        write_dns_name(&mut out, "foo.bar");
        assert_eq!(out, b"\x03foo\x03bar\x00");
    }

    #[test]
    fn write_empty_name() {
        let mut out = Vec::new();
        write_dns_name(&mut out, "");
        assert_eq!(out, b"\x00");
    }

    #[test]
    fn write_parse_round_trip() {
        let mut buf = Vec::new();
        write_dns_name(&mut buf, "my.service.stouter.local");
        let (name, end) = parse_dns_name(&buf, 0).unwrap();
        assert_eq!(name, "my.service.stouter.local");
        assert_eq!(end, buf.len());
    }

    // -- response builders ----------------------------------------------------

    #[test]
    fn nxdomain_response_header() {
        let resp = build_nxdomain_response(0x1234, true, "foo.stouter.local");
        // ID
        assert_eq!(resp[0], 0x12);
        assert_eq!(resp[1], 0x34);
        // RCODE = 3
        assert_eq!(resp[3] & 0x0F, 3);
        // RD copied
        assert_eq!(resp[2] & 0x01, 1);
        // ANCOUNT = 0
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 0);
    }

    #[test]
    fn a_response_contains_ip() {
        let resp = build_a_response(0xABCD, false, "svc.stouter.local", [127, 0, 0, 1], 8080);
        // ID
        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), 0xABCD);
        // ANCOUNT = 1
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1);
        // ARCOUNT = 1 (SRV in additional section)
        assert_eq!(u16::from_be_bytes([resp[10], resp[11]]), 1);
        // The response should contain 127.0.0.1 somewhere in the answer
        assert!(resp.windows(4).any(|w| w == [127, 0, 0, 1]));
    }

    #[test]
    fn srv_response_contains_port() {
        let resp = build_srv_response(0x0001, true, "svc.stouter.local", 9090);
        // ANCOUNT = 1
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1);
        // The port 9090 (0x2382) should appear in the SRV RDATA
        let port_bytes = 9090u16.to_be_bytes();
        assert!(resp.windows(2).any(|w| w == port_bytes));
    }

    // -- lookup_service -------------------------------------------------------

    fn make_services(entries: &[(&str, u16, &[&str])]) -> HashMap<String, ServiceInfo> {
        entries
            .iter()
            .map(|(name, port, domains)| {
                (
                    name.to_string(),
                    ServiceInfo {
                        port: *port,
                        domains: domains.iter().map(|d| d.to_string()).collect(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn lookup_by_stouter_local() {
        let svcs = make_services(&[("web", 8080, &[])]);
        assert_eq!(lookup_service(&svcs, "web.stouter.local"), Some(8080));
    }

    #[test]
    fn lookup_by_custom_domain() {
        let svcs = make_services(&[("web", 8080, &["example.com", "www.example.com"])]);
        assert_eq!(lookup_service(&svcs, "example.com"), Some(8080));
        assert_eq!(lookup_service(&svcs, "www.example.com"), Some(8080));
    }

    #[test]
    fn lookup_custom_domain_case_insensitive() {
        let svcs = make_services(&[("web", 8080, &["Example.COM"])]);
        assert_eq!(lookup_service(&svcs, "example.com"), Some(8080));
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let svcs = make_services(&[("web", 8080, &["example.com"])]);
        assert_eq!(lookup_service(&svcs, "unknown.com"), None);
        assert_eq!(lookup_service(&svcs, "other.stouter.local"), None);
    }
}
