use std::io::Read;

/// Read raw demo bytes from `path`, or stdin when `path == "-"` (so a decompression
/// script can pipe a .dem straight in). The parser needs the whole buffer anyway,
/// so stdin is read to EOF.
pub fn load_demo_bytes(path: &str) -> anyhow::Result<Vec<u8>> {
    if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin().lock().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(std::fs::read(path)?)
    }
}
