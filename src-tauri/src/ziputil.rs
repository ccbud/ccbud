// Minimal ZIP reader/writer for conversation bundles. Rust port of src/main/zipStore.js — the byte
// layout is proven there by test/zip.test.js (round-trip + system `unzip`), so this mirror stays in
// lockstep with it.
//
// A conversation with subagents exports as a .zip whose FIRST level is the main session .jsonl and
// whose `subagents/` directory holds the per-subagent files; re-importing restores that layout.
// Only the round-trip slice of the spec is implemented:
//   - write: STORE or raw-DEFLATE per entry (whichever is smaller), no zip64, no data descriptors.
//   - read : parse via the central directory (so OS-repacked zips with data descriptors still read),
//            handling STORE (0) and DEFLATE (8); unreadable members are skipped, never panic.

#![allow(dead_code)]

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use std::io::{Read, Write};

pub struct Entry {
    pub name: String,
    pub data: Vec<u8>,
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn deflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).ok()?;
    enc.finish().ok()
}

/// Build a .zip from entries. STORE unless raw-DEFLATE is strictly smaller.
pub fn build(entries: &[Entry]) -> Vec<u8> {
    let mut local: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let mut offset: u32 = 0;
    for e in entries {
        let name = e.name.as_bytes();
        let crc = crc32(&e.data);
        let deflated = deflate(&e.data);
        let (method, payload): (u16, &[u8]) = match &deflated {
            Some(d) if d.len() < e.data.len() => (8, d.as_slice()),
            _ => (0, e.data.as_slice()),
        };
        let comp_size = payload.len() as u32;
        let uncomp_size = e.data.len() as u32;

        // local file header
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes()); // version needed
        local.extend_from_slice(&0u16.to_le_bytes()); // flags
        local.extend_from_slice(&method.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // mod time
        local.extend_from_slice(&0x21u16.to_le_bytes()); // mod date = 1980-01-01
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&comp_size.to_le_bytes());
        local.extend_from_slice(&uncomp_size.to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // extra length
        local.extend_from_slice(name);
        local.extend_from_slice(payload);

        // central directory header
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central.extend_from_slice(&0x21u16.to_le_bytes()); // mod date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&comp_size.to_le_bytes());
        central.extend_from_slice(&uncomp_size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra length
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes()); // relative offset of local header
        central.extend_from_slice(name);

        offset += 30 + name.len() as u32 + comp_size;
    }
    let central_start = offset;
    let central_size = central.len() as u32;
    let mut out = local;
    out.extend_from_slice(&central);
    // end of central directory record
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with central dir
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // entries this disk
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // total entries
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out
}

fn rd_u16(buf: &[u8], at: usize) -> Option<u16> {
    buf.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn rd_u32(buf: &[u8], at: usize) -> Option<u32> {
    buf.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parse a .zip → entries. Best-effort: unreadable/unsupported members are skipped.
pub fn read(buf: &[u8]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    if buf.len() < 22 {
        return out;
    }
    // Locate the End Of Central Directory record by scanning backwards for its signature.
    let mut eocd: Option<usize> = None;
    let top = buf.len() - 22;
    let floor = top.saturating_sub(65535);
    let mut i = top;
    loop {
        if rd_u32(buf, i) == Some(0x0605_4b50) {
            eocd = Some(i);
            break;
        }
        if i <= floor {
            break;
        }
        i -= 1;
    }
    let eocd = match eocd {
        Some(e) => e,
        None => return out,
    };
    let count = rd_u16(buf, eocd + 10).unwrap_or(0) as usize;
    let mut p = rd_u32(buf, eocd + 16).unwrap_or(0) as usize; // central directory offset
    for _ in 0..count {
        if rd_u32(buf, p) != Some(0x0201_4b50) {
            break;
        }
        let method = rd_u16(buf, p + 10).unwrap_or(0);
        let comp_size = rd_u32(buf, p + 20).unwrap_or(0) as usize;
        let name_len = rd_u16(buf, p + 28).unwrap_or(0) as usize;
        let extra_len = rd_u16(buf, p + 30).unwrap_or(0) as usize;
        let comment_len = rd_u16(buf, p + 32).unwrap_or(0) as usize;
        let local_off = rd_u32(buf, p + 42).unwrap_or(0) as usize;
        let name = buf
            .get(p + 46..(p + 46 + name_len).min(buf.len()))
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .unwrap_or_default();
        // The local header repeats name/extra lengths; trust it for the data offset.
        if rd_u32(buf, local_off) == Some(0x0403_4b50) {
            let lh_name = rd_u16(buf, local_off + 26).unwrap_or(0) as usize;
            let lh_extra = rd_u16(buf, local_off + 28).unwrap_or(0) as usize;
            let data_start = local_off + 30 + lh_name + lh_extra;
            let data_end = data_start + comp_size;
            if data_end <= buf.len() {
                let payload = &buf[data_start..data_end];
                let data = match method {
                    0 => Some(payload.to_vec()),
                    8 => {
                        let mut v = Vec::new();
                        DeflateDecoder::new(payload).read_to_end(&mut v).ok().map(|_| v)
                    }
                    _ => None,
                };
                if let Some(data) = data {
                    out.push(Entry { name, data });
                }
            }
        }
        p += 46 + name_len + extra_len + comment_len;
    }
    out
}

fn norm(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches("./").to_string()
}
fn in_subagents(name: &str) -> bool {
    norm(name).split('/').any(|seg| seg == "subagents")
}
fn depth(name: &str) -> usize {
    norm(name).matches('/').count()
}
fn base_name(name: &str) -> String {
    norm(name).split('/').filter(|s| !s.is_empty()).last().unwrap_or("").to_string()
}

/// Split a bundle's entries into (main, subagents), mirroring zipStore.js splitBundle: the main
/// session is the shallowest top-level *.jsonl (never under a subagents/ segment); subagents are the
/// agent-* transcript / meta files under any subagents/ directory. Tolerant of a wrapping folder.
/// Returns (Some((name, data)), Vec<(name, data)>); main is None when no session file is present.
pub fn split_bundle(entries: Vec<Entry>) -> (Option<(String, Vec<u8>)>, Vec<(String, Vec<u8>)>) {
    let mut main: Option<usize> = None;
    for (i, e) in entries.iter().enumerate() {
        if !e.name.to_lowercase().ends_with(".jsonl") || in_subagents(&e.name) {
            continue;
        }
        match main {
            Some(m) if depth(&entries[m].name) <= depth(&e.name) => {}
            _ => main = Some(i),
        }
    }
    let mut subagents: Vec<(String, Vec<u8>)> = Vec::new();
    for e in &entries {
        if !in_subagents(&e.name) {
            continue;
        }
        let base = base_name(&e.name);
        let lower = base.to_lowercase();
        if lower.starts_with("agent-") && (lower.ends_with(".jsonl") || lower.ends_with(".meta.json")) {
            subagents.push((base, e.data.clone()));
        }
    }
    let main_out = main.map(|i| (base_name(&entries[i].name), entries[i].data.clone()));
    (main_out, subagents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_value() {
        // Standard CRC-32 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn round_trips_store_and_deflate() {
        let big = b"{\"type\":\"assistant\"}\n".repeat(4000); // very compressible → DEFLATE
        let bin = vec![0u8, 1, 2, 3, 255, 254, 10, 13, 0, 42];
        let entries = vec![
            Entry { name: "main.jsonl".into(), data: b"hi\n".to_vec() },
            Entry { name: "subagents/agent-aaa.jsonl".into(), data: big.clone() },
            Entry { name: "subagents/agent-aaa.meta.json".into(), data: b"{\"toolUseId\":\"tu1\"}".to_vec() },
            Entry { name: "blob.bin".into(), data: bin.clone() },
        ];
        let zip = build(&entries);
        assert_eq!(u32::from_le_bytes([zip[0], zip[1], zip[2], zip[3]]), 0x0403_4b50);
        assert!(zip.len() < big.len(), "deflate should shrink: zip={} raw={}", zip.len(), big.len());

        let read_back = read(&zip);
        assert_eq!(read_back.len(), entries.len());
        for src in &entries {
            let got = read_back.iter().find(|r| r.name == src.name).expect("entry present");
            assert_eq!(got.data, src.data, "payload mismatch for {}", src.name);
        }
    }

    #[test]
    fn split_bundle_recovers_main_and_subagents() {
        let entries = vec![
            Entry { name: "main.jsonl".into(), data: b"m".to_vec() },
            Entry { name: "subagents/agent-aaa.jsonl".into(), data: b"a".to_vec() },
            Entry { name: "subagents/agent-aaa.meta.json".into(), data: b"{}".to_vec() },
            Entry { name: "blob.bin".into(), data: b"x".to_vec() },
        ];
        let (main, subs) = split_bundle(entries);
        assert_eq!(main.as_ref().map(|(n, _)| n.as_str()), Some("main.jsonl"));
        assert_eq!(subs.len(), 2);
        assert!(subs.iter().all(|(n, _)| !n.contains('/')));
        assert!(subs.iter().any(|(n, _)| n == "agent-aaa.meta.json"));
    }

    #[test]
    fn split_bundle_tolerates_wrapping_folder() {
        let entries = vec![
            Entry { name: "bundle/sess.jsonl".into(), data: b"m".to_vec() },
            Entry { name: "bundle/subagents/agent-x.jsonl".into(), data: b"a".to_vec() },
        ];
        let (main, subs) = split_bundle(entries);
        assert_eq!(main.as_ref().map(|(n, _)| n.as_str()), Some("sess.jsonl"));
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn read_tolerates_garbage() {
        assert_eq!(read(b"not a zip at all").len(), 0);
        assert_eq!(read(&[]).len(), 0);
    }
}
