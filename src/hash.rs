//! SHA-256 (x86 SHA extensions when present, portable otherwise) and CRC-32 (zlib's), std only.

pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

static K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Default for Sha256 {
    fn default() -> Self {
        Sha256 {
            state: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19],
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }
}

impl Sha256 {
    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 64 {
                return;
            }
            let block = self.buf;
            compress(&mut self.state, &block);
            self.buf_len = 0;
        }
        let whole = data.len() / 64 * 64;
        if whole > 0 {
            compress(&mut self.state, &data[..whole]);
        }
        let rest = &data[whole..];
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        while (self.buf_len + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad);
        self.total = total;
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    pub fn hex(self) -> String {
        hex(&self.finish())
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::default();
    h.update(data);
    h.hex()
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

fn compress(state: &mut [u32; 8], blocks: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sha") && std::arch::is_x86_feature_detected!("sse4.1") {
            // SAFETY: the required CPU features were detected at run time.
            unsafe { shani::compress(state, blocks) };
            return;
        }
    }
    for block in blocks.chunks_exact(64) {
        compress_portable(state, block);
    }
}

fn compress_portable(state: &mut [u32; 8], block: &[u8]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *s = s.wrapping_add(v);
    }
}

#[cfg(target_arch = "x86_64")]
mod shani {
    use std::arch::x86_64::*;

    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    pub unsafe fn compress(state: &mut [u32; 8], blocks: &[u8]) {
        let mask = _mm_set_epi64x(0x0c0d_0e0f_0809_0a0b, 0x0405_0607_0001_0203);
        let mut tmp = _mm_loadu_si128(state.as_ptr() as *const __m128i); // DCBA
        let mut state1 = _mm_loadu_si128(state.as_ptr().add(4) as *const __m128i); // HGFE
        tmp = _mm_shuffle_epi32(tmp, 0xB1); // CDAB
        state1 = _mm_shuffle_epi32(state1, 0x1B); // EFGH
        let mut state0 = _mm_alignr_epi8(tmp, state1, 8); // ABEF
        state1 = _mm_blend_epi16(state1, tmp, 0xF0); // CDGH
        let k = super::K.as_ptr() as *const __m128i;
        for block in blocks.chunks_exact(64) {
            let abef_save = state0;
            let cdgh_save = state1;
            let p = block.as_ptr() as *const __m128i;
            let mut m = [
                _mm_shuffle_epi8(_mm_loadu_si128(p), mask),
                _mm_shuffle_epi8(_mm_loadu_si128(p.add(1)), mask),
                _mm_shuffle_epi8(_mm_loadu_si128(p.add(2)), mask),
                _mm_shuffle_epi8(_mm_loadu_si128(p.add(3)), mask),
            ];
            for round in 0..16 {
                let i = round % 4;
                if round >= 4 {
                    // m[i] = sha256msg2(sha256msg1(m[i], m[i+1]) + alignr(m[i+3], m[i+2], 4), m[i+3])
                    let t = _mm_sha256msg1_epu32(m[i], m[(i + 1) % 4]);
                    let t = _mm_add_epi32(t, _mm_alignr_epi8(m[(i + 3) % 4], m[(i + 2) % 4], 4));
                    m[i] = _mm_sha256msg2_epu32(t, m[(i + 3) % 4]);
                }
                let msg = _mm_add_epi32(m[i], _mm_loadu_si128(k.add(round)));
                state1 = _mm_sha256rnds2_epu32(state1, state0, msg);
                state0 = _mm_sha256rnds2_epu32(state0, state1, _mm_shuffle_epi32(msg, 0x0E));
            }
            state0 = _mm_add_epi32(state0, abef_save);
            state1 = _mm_add_epi32(state1, cdgh_save);
        }
        tmp = _mm_shuffle_epi32(state0, 0x1B); // FEBA
        state1 = _mm_shuffle_epi32(state1, 0xB1); // DCHG
        state0 = _mm_blend_epi16(tmp, state1, 0xF0); // DCBA
        state1 = _mm_alignr_epi8(state1, tmp, 8); // ABEF -> HGFE
        _mm_storeu_si128(state.as_mut_ptr() as *mut __m128i, state0);
        _mm_storeu_si128(state.as_mut_ptr().add(4) as *mut __m128i, state1);
    }
}

// ----------------------------------------------------------------------------------- CRC-32 ---

/// zlib `crc32` (reflected polynomial 0xEDB88320), slicing-by-16.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let t = crc_tables();
    let mut c = !crc;
    let mut chunks = data.chunks_exact(16);
    for chunk in &mut chunks {
        let a = c ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        c = t[15][(a & 0xFF) as usize] ^ t[14][((a >> 8) & 0xFF) as usize] ^ t[13][((a >> 16) & 0xFF) as usize] ^ t[12][(a >> 24) as usize]
            ^ t[11][chunk[4] as usize] ^ t[10][chunk[5] as usize] ^ t[9][chunk[6] as usize] ^ t[8][chunk[7] as usize]
            ^ t[7][chunk[8] as usize] ^ t[6][chunk[9] as usize] ^ t[5][chunk[10] as usize] ^ t[4][chunk[11] as usize]
            ^ t[3][chunk[12] as usize] ^ t[2][chunk[13] as usize] ^ t[1][chunk[14] as usize] ^ t[0][chunk[15] as usize];
    }
    for &b in chunks.remainder() {
        c = t[0][((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

fn crc_tables() -> &'static [[u32; 256]; 16] {
    use std::sync::OnceLock;
    static TABLES: OnceLock<Box<[[u32; 256]; 16]>> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut t = Box::new([[0u32; 256]; 16]);
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            t[0][i as usize] = c;
        }
        for i in 0..256 {
            for k in 1..16 {
                let prev = t[k - 1][i];
                t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            }
        }
        t
    })
}

/// Running prefix check: SHA-256 when a paging snapshot pins it, otherwise CRC-32.
pub enum PrefixCheck {
    Strong(Sha256),
    Crc(u32),
}

impl PrefixCheck {
    pub fn new(strong: bool) -> PrefixCheck {
        if strong {
            PrefixCheck::Strong(Sha256::default())
        } else {
            PrefixCheck::Crc(0)
        }
    }
    pub fn update(&mut self, data: &[u8]) {
        match self {
            PrefixCheck::Strong(h) => h.update(data),
            PrefixCheck::Crc(c) => *c = crc32_update(*c, data),
        }
    }
    pub fn value(self) -> String {
        match self {
            PrefixCheck::Strong(h) => h.hex(),
            PrefixCheck::Crc(c) => format!("crc32:{:08x}", c),
        }
    }
}
