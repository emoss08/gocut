use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

pub(crate) const NUM_SECTIONS: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum Section {
    String = 0,
    Meta = 1,
    PosBase = 2,
    Pkg = 3,
    Name = 4,
    Type = 5,
    Obj = 6,
    ObjExt = 7,
    ObjDict = 8,
    Body = 9,
}

impl Section {
    pub(crate) fn from_u64(v: u64) -> Result<Section> {
        Ok(match v {
            0 => Section::String,
            1 => Section::Meta,
            2 => Section::PosBase,
            3 => Section::Pkg,
            4 => Section::Name,
            5 => Section::Type,
            6 => Section::Obj,
            7 => Section::ObjExt,
            8 => Section::ObjDict,
            9 => Section::Body,
            _ => bail!("bad section kind {v}"),
        })
    }
}

pub(crate) const VERSION_FLAGS: u32 = 1;
pub(crate) const VERSION_GENERIC_METHODS: u32 = 4;
const FLAG_SYNC_MARKERS: u32 = 1;

/// The export data versions gocut's masking and patching have been verified against (Go 1.27's
/// unified IR). Any other version is refused, so the package is compiled normally: a later Go may
/// change what a field means without changing its layout, which masking would not notice.
pub(crate) const SUPPORTED_VERSIONS: &[u32] = &[VERSION_GENERIC_METHODS];

pub(crate) struct PkgBits<'a> {
    pub version: u32,
    pub flags: u32,
    pub ends_ends: [u32; NUM_SECTIONS],
    pub elem_ends: Vec<u32>,
    pub data: &'a [u8],
    pub fingerprint: [u8; 8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Reloc {
    pub kind: Section,
    pub idx: u32,
}

pub(crate) struct Elem<'a> {
    pub relocs: Vec<Reloc>,
    pub payload: &'a [u8],
}

fn read_u32(b: &[u8], off: usize) -> Result<u32> {
    let s = b.get(off..off + 4).context("truncated export data header")?;
    Ok(u32::from_le_bytes(s.try_into().expect("a 4-byte slice converts to [u8; 4]")))
}

impl<'a> PkgBits<'a> {
    pub(crate) fn parse(input: &'a [u8]) -> Result<PkgBits<'a>> {
        let mut off = 0;
        let version = read_u32(input, off)?;
        if !SUPPORTED_VERSIONS.contains(&version) {
            bail!("export data version {version} is not one gocut supports ({SUPPORTED_VERSIONS:?})");
        }
        off += 4;
        let mut flags = 0;
        if version >= VERSION_FLAGS {
            flags = read_u32(input, off)?;
            off += 4;
        }
        if flags & FLAG_SYNC_MARKERS != 0 {
            bail!("export data has sync markers; unsupported");
        }
        let mut ends_ends = [0u32; NUM_SECTIONS];
        for e in &mut ends_ends {
            *e = read_u32(input, off)?;
            off += 4;
        }
        ensure!(ends_ends.windows(2).all(|w| w[0] <= w[1]), "export data section ends out of order");
        let total = ends_ends[NUM_SECTIONS - 1] as usize;
        let ends = input.get(off..off + 4 * total).context("truncated export data header")?;
        let elem_ends = ends.as_chunks::<4>().0.iter().map(|c| read_u32(c, 0)).collect::<Result<Vec<u32>>>()?;
        off += ends.len();
        ensure!(elem_ends.windows(2).all(|w| w[0] <= w[1]), "export data element ends out of order");
        ensure!(input.len() >= off + 8, "truncated export data");
        let data = &input[off..input.len() - 8];
        let last = elem_ends.last().copied().unwrap_or(0) as usize;
        ensure!(data.len() == last, "export data length mismatch");
        let mut fingerprint = [0u8; 8];
        fingerprint.copy_from_slice(&input[input.len() - 8..]);
        Ok(PkgBits { version, flags, ends_ends, elem_ends, data, fingerprint })
    }

    pub(crate) fn num(&self, k: Section) -> usize {
        let k = k as usize;
        let end = self.ends_ends[k] as usize;
        let start = if k > 0 { self.ends_ends[k - 1] as usize } else { 0 };
        end - start
    }

    pub(crate) fn abs(&self, k: Section, idx: u32) -> Result<usize> {
        let ki = k as usize;
        let base = if ki > 0 { self.ends_ends[ki - 1] as usize } else { 0 };
        let a = base + idx as usize;
        ensure!(a < self.ends_ends[ki] as usize, "{k:?}:{idx} out of bounds");
        Ok(a)
    }

    pub(crate) fn section_of(&self, a: usize) -> Option<(Section, u32)> {
        let k = self.ends_ends.iter().position(|&e| a < e as usize)?;
        let base = if k > 0 { self.ends_ends[k - 1] as usize } else { 0 };
        Some((Section::from_u64(k as u64).ok()?, u32::try_from(a - base).ok()?))
    }

    pub(crate) fn raw_abs(&self, a: usize) -> &'a [u8] {
        let start = if a > 0 { self.elem_ends[a - 1] as usize } else { 0 };
        &self.data[start..self.elem_ends[a] as usize]
    }

    pub(crate) fn raw(&self, k: Section, idx: u32) -> Result<&'a [u8]> {
        Ok(self.raw_abs(self.abs(k, idx)?))
    }

    pub(crate) fn string(&self, idx: u32) -> Result<&'a [u8]> {
        self.raw(Section::String, idx)
    }

    pub(crate) fn elem(&self, k: Section, idx: u32) -> Result<Elem<'a>> {
        ensure!(k != Section::String, "string elements have no reloc header");
        decode_elem(self.raw(k, idx)?)
    }
}

pub(crate) fn decode_elem(raw: &[u8]) -> Result<Elem<'_>> {
    let mut r = Rd::new(raw);
    let n = usize::try_from(r.uvarint()?)?;
    let mut relocs = Vec::with_capacity(n.min(raw.len() / 2));
    for _ in 0..n {
        let kind = Section::from_u64(r.uvarint()?)?;
        let idx = u32::try_from(r.uvarint()?).context("reloc index out of range")?;
        relocs.push(Reloc { kind, idx });
    }
    Ok(Elem { relocs, payload: &raw[r.pos..] })
}

pub(crate) fn encode_elem(relocs: &[Reloc], payload: &[u8], out: &mut Vec<u8>) {
    put_uvarint(out, relocs.len() as u64);
    for r in relocs {
        put_uvarint(out, r.kind as u64);
        put_uvarint(out, u64::from(r.idx));
    }
    out.extend_from_slice(payload);
}

pub(crate) struct Rd<'a> {
    pub b: &'a [u8],
    pub pos: usize,
}

impl<'a> Rd<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Rd<'a> {
        Rd { b, pos: 0 }
    }

    pub(crate) fn uvarint(&mut self) -> Result<u64> {
        let mut x: u64 = 0;
        let mut s = 0u32;
        for i in 0..10 {
            let Some(&c) = self.b.get(self.pos) else { bail!("truncated varint") };
            self.pos += 1;
            if c < 0x80 {
                if i == 9 && c > 1 {
                    bail!("varint overflow");
                }
                return Ok(x | u64::from(c) << s);
            }
            x |= u64::from(c & 0x7f) << s;
            s += 7;
        }
        bail!("varint overflow")
    }

    pub(crate) fn varint(&mut self) -> Result<i64> {
        let ux = self.uvarint()?;
        let mut x = (ux >> 1).cast_signed();
        if ux & 1 != 0 {
            x = !x;
        }
        Ok(x)
    }

    pub(crate) fn boolean(&mut self) -> Result<bool> {
        let Some(&c) = self.b.get(self.pos) else { bail!("truncated bool") };
        self.pos += 1;
        match c {
            0 => Ok(false),
            1 => Ok(true),
            _ => bail!("bad bool byte {c}"),
        }
    }

    pub(crate) fn skip_uvarint(&mut self) -> Result<()> {
        self.uvarint().map(|_| ())
    }

    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.b.len()
    }
}

pub(crate) fn put_uvarint(out: &mut Vec<u8>, mut x: u64) {
    // Each byte carries the low seven bits of what is left.
    while x >= 0x80 {
        out.push(x.to_le_bytes()[0] | 0x80);
        x >>= 7;
    }
    out.push(x.to_le_bytes()[0]);
}

pub(crate) fn dump(version: u32, flags: u32, ends_ends: &[u32; NUM_SECTIONS], elems: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(&version.to_le_bytes());
    if version >= VERSION_FLAGS {
        out.extend_from_slice(&flags.to_le_bytes());
    }
    for e in ends_ends {
        out.extend_from_slice(&e.to_le_bytes());
    }
    let mut sum = 0u32;
    for e in elems {
        sum = u32::try_from(e.len())
            .ok()
            .and_then(|n| sum.checked_add(n))
            .context("export data element ends overflow u32")?;
        out.extend_from_slice(&sum.to_le_bytes());
    }
    for e in elems {
        out.extend_from_slice(e);
    }
    let fp = Sha256::digest(&out);
    out.extend_from_slice(&fp[..8]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_unsupported_versions() {
        for v in [0u32, 1, 2, 3, 5] {
            let mut input = v.to_le_bytes().to_vec();
            input.extend_from_slice(&[0u8; 64]);
            let err = PkgBits::parse(&input).err().expect("parse should refuse");
            assert!(err.to_string().contains("not one gocut supports"), "version {v}: {err}");
        }
    }
}
