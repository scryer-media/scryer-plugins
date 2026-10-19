//! Packaging without xtask: append the `scryer.plugin-descriptor.v1` custom
//! section that Scryer requires on an indexer component.
//!
//! This mirrors the repository xtask byte for byte: the payload is the JSON
//! document the component's own `describe` export returns, with the legacy
//! `scryer_describe` marker escaped, written as a top-level custom section
//! (id 0, LEB128 size, LEB128 name length, name, payload) at the end of the
//! component. It exists only because the xtask's SDK cannot yet read this
//! plugin's descriptor; the ignored test `write_embedded_artifact` runs it.

use scryer_plugin_sdk::EXPORT_DESCRIBE;

pub const SECTION_NAME: &str = "scryer.plugin-descriptor.v1";
const SECTION_PREFIX: &str = "scryer.plugin-descriptor.";
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const ESCAPED_LEGACY_DESCRIBE_MARKER: &str = r"scryer_describe";

/// The document `describe` returns, as the artifact carries it.
pub fn descriptor_payload() -> Vec<u8> {
    let document = scryer_plugin_pdk::component::descriptor_bytes(crate::build_descriptor());
    String::from_utf8(document)
        .expect("descriptor JSON is UTF-8")
        .replace(EXPORT_DESCRIBE, ESCAPED_LEGACY_DESCRIBE_MARKER)
        .into_bytes()
}

fn is_core(wasm: &[u8]) -> bool {
    wasm.starts_with(b"\0asm\x01\0\0\0")
}

fn is_component(wasm: &[u8]) -> bool {
    wasm.starts_with(b"\0asm\r\0\x01\0")
}

fn read_leb(bytes: &[u8], offset: &mut usize) -> Result<u32, String> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*offset).ok_or("truncated LEB128")?;
        *offset += 1;
        if shift == 28 && byte & 0xf0 != 0 {
            return Err("LEB128 exceeds u32".into());
        }
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("invalid LEB128".into())
}

fn write_leb(mut value: u32, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Every top-level descriptor section: `(name, payload)`.
pub fn descriptor_sections(wasm: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    if !is_core(wasm) && !is_component(wasm) {
        return Err("not a WebAssembly module or component".into());
    }
    let mut offset = 8usize;
    let mut found = Vec::new();
    while offset < wasm.len() {
        let id = wasm[offset];
        offset += 1;
        let len = read_leb(wasm, &mut offset)? as usize;
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= wasm.len())
            .ok_or("truncated section")?;
        if id == 0 {
            let mut cursor = offset;
            let name_len = read_leb(wasm, &mut cursor)? as usize;
            let name_end = cursor
                .checked_add(name_len)
                .filter(|name_end| *name_end <= end)
                .ok_or("truncated custom section name")?;
            let name = std::str::from_utf8(&wasm[cursor..name_end])
                .map_err(|_| "custom section name is not UTF-8")?;
            if name.starts_with(SECTION_PREFIX) {
                found.push((name.to_string(), wasm[name_end..end].to_vec()));
            }
        }
        offset = end;
    }
    Ok(found)
}

pub fn append_descriptor_section(wasm: &[u8], payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(format!(
            "descriptor exceeds the {MAX_PAYLOAD_BYTES} byte limit"
        ));
    }
    if !descriptor_sections(wasm)?.is_empty() {
        return Err("artifact already carries a descriptor section".into());
    }
    let name = SECTION_NAME.as_bytes();
    let mut body = Vec::with_capacity(name.len() + payload.len() + 5);
    write_leb(name.len() as u32, &mut body);
    body.extend_from_slice(name);
    body.extend_from_slice(payload);
    let mut embedded = wasm.to_vec();
    embedded.push(0);
    write_leb(
        u32::try_from(body.len()).map_err(|_| "section too large")?,
        &mut embedded,
    );
    embedded.extend_from_slice(&body);
    Ok(embedded)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPONENT_HEADER: &[u8] = b"\0asm\r\0\x01\0";

    #[test]
    fn section_round_trips_on_a_synthetic_component() {
        // A header plus one unrelated custom section.
        let mut wasm = COMPONENT_HEADER.to_vec();
        wasm.extend_from_slice(&[0, 4, 3, b'f', b'o', b'o']);
        let payload = br#"{"id":"mediathekviewweb"}"#;
        let embedded = append_descriptor_section(&wasm, payload).unwrap();
        assert!(embedded.starts_with(&wasm));
        assert_eq!(
            descriptor_sections(&embedded).unwrap(),
            vec![(SECTION_NAME.to_string(), payload.to_vec())]
        );
        // A second embed is refused.
        assert!(append_descriptor_section(&embedded, payload).is_err());
    }

    #[test]
    fn multi_byte_section_lengths_round_trip() {
        let payload = vec![b'x'; 300];
        let embedded = append_descriptor_section(COMPONENT_HEADER, &payload).unwrap();
        assert_eq!(descriptor_sections(&embedded).unwrap()[0].1, payload);
    }

    #[test]
    fn oversized_payloads_and_non_wasm_are_refused() {
        assert!(
            append_descriptor_section(COMPONENT_HEADER, &vec![0; MAX_PAYLOAD_BYTES + 1]).is_err()
        );
        assert!(descriptor_sections(b"not wasm").is_err());
    }

    #[test]
    fn payload_is_the_describe_document() {
        let payload = descriptor_payload();
        let document = scryer_plugin_pdk::component::descriptor_bytes(crate::build_descriptor());
        assert_eq!(payload, document, "no legacy marker to escape");
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value["id"], "mediathekviewweb");
        assert!(payload.len() <= MAX_PAYLOAD_BYTES);
    }

    /// Writes `mediathekviewweb_indexer.embedded.wasm` beside the plugin-release
    /// build. Run explicitly after the wasm build:
    /// `cargo nextest run --locked --lib --run-ignored only write_embedded_artifact`.
    #[test]
    #[ignore = "packaging step; needs the plugin-release wasm"]
    fn write_embedded_artifact() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/wasm32-wasip2/plugin-release");
        let wasm = std::fs::read(dir.join("mediathekviewweb_indexer.wasm")).expect("built wasm");
        assert!(is_component(&wasm));
        let payload = descriptor_payload();
        let embedded = append_descriptor_section(&wasm, &payload).unwrap();
        assert_eq!(
            descriptor_sections(&embedded).unwrap(),
            vec![(SECTION_NAME.to_string(), payload)]
        );
        std::fs::write(dir.join("mediathekviewweb_indexer.embedded.wasm"), embedded).unwrap();
    }
}
