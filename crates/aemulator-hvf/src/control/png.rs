//! A tiny, dependency-free PNG encoder — just enough to serialize the guest scanout framebuffer
//! as a real 8-bit RGB PNG from the `control` plane's `screenshot` command.
//!
//! **Why hand-rolled:** the crate must stay hermetic (offline default build, no new registry deps),
//! and none of `png`/`image`/`flate2` are vendored. So we emit a valid PNG whose IDAT is a zlib
//! stream built ENTIRELY from DEFLATE **stored** (uncompressed, BTYPE=00) blocks — no compressor
//! needed. The file is larger than a compressed PNG but byte-for-byte a spec-conformant PNG that any
//! viewer (Preview, Chrome, `sips`) opens. We implement the two checksums the containers need:
//! CRC-32 (PNG chunk trailer, poly 0xEDB88320) and Adler-32 (zlib stream trailer).

/// Encode `rgb` (`width*height*3` bytes, row-major, 8-bit R,G,B) as a PNG byte stream.
pub fn encode_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(
        rgb.len(),
        (width as usize) * (height as usize) * 3,
        "rgb size mismatch"
    );

    let mut out = Vec::with_capacity(rgb.len() + rgb.len() / 100 + 4096);
    // PNG signature.
    out.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    // IHDR: width, height, bit depth 8, color type 2 (truecolor RGB), no compression/filter/interlace.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(2); // color type = truecolor
    ihdr.push(0); // compression method = deflate
    ihdr.push(0); // filter method = adaptive (per-row filter byte)
    ihdr.push(0); // interlace = none
    write_chunk(&mut out, b"IHDR", &ihdr);

    // Build the raw (pre-compression) image stream: each row prefixed with filter byte 0 (None).
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0); // filter type: None
        raw.extend_from_slice(&rgb[y * stride..(y + 1) * stride]);
    }

    let idat = zlib_stored(&raw);
    write_chunk(&mut out, b"IDAT", &idat);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// Wrap `data` in a zlib stream whose payload is a sequence of DEFLATE **stored** blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut z = Vec::with_capacity(data.len() + data.len() / 65535 * 5 + 16);
    // zlib header: CMF=0x78 (deflate, 32K window), FLG=0x01 → (0x78*256+0x01) % 31 == 0, no dict.
    z.push(0x78);
    z.push(0x01);

    // Stored blocks: max 65535 bytes each. Header byte = BFINAL(bit0) | BTYPE(bits1..2 = 00).
    let mut i = 0usize;
    let n = data.len();
    if n == 0 {
        // A single empty final stored block.
        z.push(0x01);
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(&(!0u16).to_le_bytes());
    } else {
        while i < n {
            let chunk = core::cmp::min(65535, n - i);
            let final_block = i + chunk >= n;
            z.push(if final_block { 0x01 } else { 0x00 });
            let len = chunk as u16;
            z.extend_from_slice(&len.to_le_bytes());
            z.extend_from_slice(&(!len).to_le_bytes());
            z.extend_from_slice(&data[i..i + chunk]);
            i += chunk;
        }
    }

    // zlib trailer: Adler-32 of the uncompressed data, big-endian.
    z.extend_from_slice(&adler32(data).to_be_bytes());
    z
}

/// Append one PNG chunk: length(BE) + type + data + CRC32(type+data, BE).
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    // Process in chunks so the running sums never overflow before the modulo.
    for block in data.chunks(5552) {
        for &byte in block {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

struct Crc32 {
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        Crc32 { value: 0xffff_ffff }
    }
    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let idx = ((self.value ^ byte as u32) & 0xff) as usize;
            self.value = CRC_TABLE[idx] ^ (self.value >> 8);
        }
    }
    fn finalize(self) -> u32 {
        self.value ^ 0xffff_ffff
    }
}

/// Standard CRC-32 (reflected, poly 0xEDB88320) lookup table, computed at first use.
static CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler32_known_vector() {
        // Adler-32 of "Wikipedia" is 0x11E60398 (canonical test vector).
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn crc32_known_vector() {
        // CRC-32 of "123456789" is 0xCBF43926.
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.finalize(), 0xCBF4_3926);
    }

    #[test]
    fn encodes_valid_png_header_and_chunks() {
        let w = 2u32;
        let h = 2u32;
        let rgb = vec![255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let png = encode_rgb(w, h, &rgb);
        // Signature.
        assert_eq!(&png[0..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        // IHDR type at offset 12.
        assert_eq!(&png[12..16], b"IHDR");
        // Width/height big-endian in IHDR data (offset 16).
        assert_eq!(u32::from_be_bytes([png[16], png[17], png[18], png[19]]), w);
        assert_eq!(u32::from_be_bytes([png[20], png[21], png[22], png[23]]), h);
        // Ends with IEND.
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }
}
