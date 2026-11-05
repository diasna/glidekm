//! Minimal PNG writer for clipboard bitmaps (CF_DIB -> PNG). Uses stored
//! (uncompressed) deflate blocks: no dependencies, larger output.

#![cfg_attr(not(windows), allow(dead_code))]

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(ty);
    out.extend_from_slice(body);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encode 8-bit RGB (`channels` 3) or RGBA (4) rows; `raw` has no filter bytes.
fn encode(w: u32, h: u32, channels: usize, raw: &[u8]) -> Vec<u8> {
    let stride = w as usize * channels;
    let mut scan = Vec::with_capacity(raw.len() + h as usize);
    for row in raw.chunks(stride) {
        scan.push(0); // filter: none
        scan.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = scan.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&scan).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, if channels == 4 { 6 } else { 2 }, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Convert a packed DIB (24/32-bit, uncompressed) to PNG.
pub fn from_dib(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |i: usize| Some(u32::from_le_bytes(dib.get(i..i + 4)?.try_into().ok()?));
    let hdr = u32_at(0)? as usize;
    if hdr < 40 {
        return None;
    }
    let w = u32_at(4)? as i32;
    let h = u32_at(8)? as i32;
    let bpp = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?) as usize;
    let compression = u32_at(16)?;
    let clr_used = u32_at(32)? as usize;
    if w <= 0 || h == 0 || !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3) {
        return None;
    }
    let (w, top_down, ah) = (w as usize, h < 0, h.unsigned_abs() as usize);
    if w.checked_mul(ah)? > 64 * 1024 * 1024 / 4 {
        return None;
    }
    let mut off = hdr + clr_used * 4;
    if compression == 3 && hdr == 40 {
        off += 12; // colour masks follow the v3 header
    }
    let stride = (w * bpp).div_ceil(32) * 4;
    let pix = dib.get(off..off + stride * ah)?;
    let bytes = bpp / 8;
    let has_alpha = bpp == 32
        && pix
            .chunks(stride)
            .any(|r| r[..w * 4].chunks(4).any(|p| p[3] != 0));
    let ch = if has_alpha { 4 } else { 3 };
    let mut raw = Vec::with_capacity(w * ah * ch);
    for y in 0..ah {
        let src = if top_down { y } else { ah - 1 - y };
        let row = &pix[src * stride..src * stride + w * bytes];
        for p in row.chunks(bytes) {
            raw.extend_from_slice(&[p[2], p[1], p[0]]);
            if has_alpha {
                raw.push(p[3]);
            }
        }
    }
    Some(encode(w as u32, ah as u32, ch, &raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn dib_to_png_shape() {
        // 2x1, 24-bit, bottom-up: pixels BGR (0,0,255) and (0,255,0), row padded to 8 bytes
        let mut dib = vec![0u8; 40];
        dib[0] = 40;
        dib[4] = 2;
        dib[8] = 1;
        dib[12] = 1;
        dib[14] = 24;
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        let png = from_dib(&dib).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(png.ends_with(b"IEND\xaeB`\x82"));
    }
}
