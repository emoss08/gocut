use std::fmt::Write;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

pub(crate) const MAGIC: &[u8] = b"\x00go120ld";

pub(crate) const BLK_AUTOLIB: usize = 0;
pub(crate) const BLK_PKGIDX: usize = 1;
pub(crate) const BLK_SYMDEF: usize = 3;
pub(crate) const BLK_HASHED64DEF: usize = 4;
pub(crate) const BLK_HASHEDDEF: usize = 5;
pub(crate) const BLK_NONPKGDEF: usize = 6;
pub(crate) const BLK_NONPKGREF: usize = 7;
pub(crate) const BLK_REFFLAGS: usize = 8;
pub(crate) const BLK_HASH64: usize = 9;
pub(crate) const BLK_HASH: usize = 10;
pub(crate) const BLK_RELOCIDX: usize = 11;
pub(crate) const BLK_AUXIDX: usize = 12;
pub(crate) const BLK_DATAIDX: usize = 13;
pub(crate) const BLK_RELOC: usize = 14;
pub(crate) const BLK_AUX: usize = 15;
pub(crate) const BLK_DATA: usize = 16;
pub(crate) const BLK_REFNAME: usize = 17;
pub(crate) const BLK_END: usize = 18;
pub(crate) const NBLK: usize = 19;

pub(crate) const PKG_IDX_NONE: u32 = (1 << 31) - 1;
pub(crate) const PKG_IDX_HASHED64: u32 = PKG_IDX_NONE - 1;
pub(crate) const PKG_IDX_HASHED: u32 = PKG_IDX_NONE - 2;
pub(crate) const PKG_IDX_BUILTIN: u32 = PKG_IDX_NONE - 3;
pub(crate) const PKG_IDX_SELF: u32 = PKG_IDX_NONE - 4;

pub(crate) const SYMREF_SIZE: usize = 8;
pub(crate) const SYM_SIZE: usize = 21;
pub(crate) const RELOC_SIZE: usize = 23;
pub(crate) const AUX_SIZE: usize = 9;
pub(crate) const REFFLAG_SIZE: usize = 10;
pub(crate) const REFNAME_SIZE: usize = 16;
pub(crate) const HASH64_SIZE: usize = 8;
// goobj.HashSize: the stored hash is SHA-256 truncated to 16 bytes.
pub(crate) const HASH_SIZE: usize = 16;
const STRREF_SIZE: usize = 8;
const AUTOLIB_SIZE: usize = 16;
const RELOC_SYM: usize = 15;

pub(crate) const AUX_FUNCINFO: u8 = 1;
pub(crate) const AUX_PCSP: u8 = 7;
pub(crate) const AUX_PCDATA: u8 = 11;

pub(crate) const STEXT: u8 = 1;
pub(crate) const STEXTFIPS: u8 = 2;

pub(crate) const SRODATA: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SymRef {
    pub pkg: u32,
    pub idx: u32,
}

impl SymRef {
    pub(crate) fn is_dep(self) -> bool {
        self.pkg != 0 && self.pkg < PKG_IDX_SELF
    }
}

pub(crate) struct Obj {
    pub start: usize,
    pub off: [u32; NBLK],
    pub n_def: usize,
    pub n_h64: usize,
    pub n_hashed: usize,
    pub n_nonpkgdef: usize,
    pub n_nonpkgref: usize,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().expect("a 4-byte slice converts to [u8; 4]"))
}

fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn block_len(off: &[u32; NBLK], blk: usize) -> usize {
    (off[blk + 1] - off[blk]) as usize
}

impl Obj {
    pub(crate) fn parse(member: &[u8]) -> Result<Obj> {
        let start = crate::export::find_bytes(member, MAGIC).context("no go object magic")?;
        let h = start + MAGIC.len() + 8 + 4;
        ensure!(member.len() >= h + 4 * NBLK, "truncated object header");
        let mut off = [0u32; NBLK];
        for (i, o) in off.iter_mut().enumerate() {
            *o = u32_at(member, h + 4 * i);
        }
        for i in 1..NBLK {
            ensure!(off[i] >= off[i - 1], "object block offsets out of order");
        }
        ensure!(start + off[BLK_END] as usize <= member.len(), "truncated object");
        let syms = |blk: usize| block_len(&off, blk) / SYM_SIZE;
        let obj = Obj {
            start,
            n_def: syms(BLK_SYMDEF),
            n_h64: syms(BLK_HASHED64DEF),
            n_hashed: syms(BLK_HASHEDDEF),
            n_nonpkgdef: syms(BLK_NONPKGDEF),
            n_nonpkgref: syms(BLK_NONPKGREF),
            off,
        };
        let idx_len = 4 * (obj.n_defined() + 1);
        ensure!(obj.block_len(BLK_HASH64) == obj.n_h64 * HASH64_SIZE, "Hash64 block size mismatch");
        ensure!(obj.block_len(BLK_HASH) == obj.n_hashed * HASH_SIZE, "Hash block size mismatch");
        ensure!(obj.block_len(BLK_RELOCIDX) == idx_len, "RelocIndex block size mismatch");
        ensure!(obj.block_len(BLK_AUXIDX) == idx_len, "AuxIndex block size mismatch");
        ensure!(obj.block_len(BLK_DATAIDX) == idx_len, "DataIndex block size mismatch");
        ensure!(obj.block_len(BLK_RELOC).is_multiple_of(RELOC_SIZE), "Reloc block size mismatch");
        ensure!(obj.block_len(BLK_AUX).is_multiple_of(AUX_SIZE), "Aux block size mismatch");
        Ok(obj)
    }

    fn block_len(&self, blk: usize) -> usize {
        block_len(&self.off, blk)
    }

    fn count(&self, blk: usize, size: usize) -> usize {
        self.block_len(blk) / size
    }

    pub(crate) fn abs(&self, blk: usize) -> usize {
        self.start + self.off[blk] as usize
    }

    pub(crate) fn fingerprint_off(&self) -> usize {
        self.start + MAGIC.len()
    }

    pub(crate) fn str_bytes<'a>(&self, b: &'a [u8], o: usize) -> &'a [u8] {
        let len = u32_at(b, o) as usize;
        let so = self.start + u32_at(b, o + 4) as usize;
        &b[so..so + len]
    }

    pub(crate) fn str_at(&self, b: &[u8], o: usize) -> String {
        String::from_utf8_lossy(self.str_bytes(b, o)).into_owned()
    }

    pub(crate) fn hash_off(&self, j: usize) -> usize {
        self.abs(BLK_HASH) + HASH_SIZE * j
    }

    pub(crate) fn autolib(&self, b: &[u8]) -> Vec<(String, usize)> {
        let s = self.abs(BLK_AUTOLIB);
        (0..self.count(BLK_AUTOLIB, AUTOLIB_SIZE))
            .map(|i| {
                let o = s + AUTOLIB_SIZE * i;
                (self.str_at(b, o), o + STRREF_SIZE)
            })
            .collect()
    }

    pub(crate) fn pkg_index_bytes<'a>(&self, b: &'a [u8]) -> Vec<&'a [u8]> {
        let s = self.abs(BLK_PKGIDX);
        (0..self.count(BLK_PKGIDX, STRREF_SIZE)).map(|i| self.str_bytes(b, s + STRREF_SIZE * i)).collect()
    }

    pub(crate) fn pkg_index(&self, b: &[u8]) -> Vec<String> {
        self.pkg_index_bytes(b).into_iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect()
    }

    pub(crate) fn n_defined(&self) -> usize {
        self.n_def + self.n_h64 + self.n_hashed + self.n_nonpkgdef
    }

    pub(crate) fn hashed_global(&self, j: usize) -> usize {
        self.n_def + self.n_h64 + j
    }

    fn nonpkg_global(&self, i: usize) -> usize {
        self.hashed_global(self.n_hashed + i)
    }

    pub(crate) fn sym_off(&self, global: usize) -> usize {
        let mut i = global;
        for (blk, n) in [
            (BLK_SYMDEF, self.n_def),
            (BLK_HASHED64DEF, self.n_h64),
            (BLK_HASHEDDEF, self.n_hashed),
            (BLK_NONPKGDEF, self.n_nonpkgdef),
        ] {
            if i < n {
                return self.abs(blk) + i * SYM_SIZE;
            }
            i -= n;
        }
        self.abs(BLK_NONPKGREF) + i * SYM_SIZE
    }

    pub(crate) fn sym_name(&self, b: &[u8], global: usize) -> String {
        self.str_at(b, self.sym_off(global))
    }

    pub(crate) fn sym_type(&self, b: &[u8], global: usize) -> u8 {
        b[self.sym_off(global) + 10]
    }

    pub(crate) fn sym_size(&self, b: &[u8], global: usize) -> u32 {
        u32_at(b, self.sym_off(global) + 13)
    }

    pub(crate) fn global_of(&self, r: SymRef) -> Option<usize> {
        match r.pkg {
            PKG_IDX_SELF => Some(r.idx as usize),
            PKG_IDX_HASHED64 => Some(self.n_def + r.idx as usize),
            PKG_IDX_HASHED => Some(self.hashed_global(r.idx as usize)),
            PKG_IDX_NONE => Some(self.nonpkg_global(r.idx as usize)),
            _ => None,
        }
    }

    fn idx_range(&self, b: &[u8], blk: usize, global: usize) -> (usize, usize) {
        let ix = self.abs(blk) + 4 * global;
        (u32_at(b, ix) as usize, u32_at(b, ix + 4) as usize)
    }

    pub(crate) fn reloc_range(&self, b: &[u8], global: usize) -> (usize, usize) {
        self.idx_range(b, BLK_RELOCIDX, global)
    }

    pub(crate) fn aux_range(&self, b: &[u8], global: usize) -> (usize, usize) {
        self.idx_range(b, BLK_AUXIDX, global)
    }

    pub(crate) fn data_range(&self, b: &[u8], global: usize) -> (usize, usize) {
        let d = self.abs(BLK_DATA);
        let (d0, d1) = self.idx_range(b, BLK_DATAIDX, global);
        (d + d0, d + d1)
    }

    pub(crate) fn total_relocs(&self) -> usize {
        self.count(BLK_RELOC, RELOC_SIZE)
    }

    pub(crate) fn total_aux(&self) -> usize {
        self.count(BLK_AUX, AUX_SIZE)
    }

    pub(crate) fn reloc_off(&self, i: usize) -> usize {
        self.abs(BLK_RELOC) + i * RELOC_SIZE
    }

    pub(crate) fn aux_off(&self, i: usize) -> usize {
        self.abs(BLK_AUX) + i * AUX_SIZE
    }

    pub(crate) fn symref_at(b: &[u8], o: usize) -> SymRef {
        SymRef { pkg: u32_at(b, o), idx: u32_at(b, o + 4) }
    }

    pub(crate) fn set_symref(b: &mut [u8], o: usize, r: SymRef) {
        put_u32(b, o, r.pkg);
        put_u32(b, o + 4, r.idx);
    }

    pub(crate) fn symref_sites(&self, b: &[u8]) -> Vec<usize> {
        let mut sites = Vec::new();
        let rf = self.abs(BLK_REFFLAGS);
        sites.extend((0..self.count(BLK_REFFLAGS, REFFLAG_SIZE)).map(|i| rf + i * REFFLAG_SIZE));
        sites.extend((0..self.total_relocs()).map(|i| self.reloc_off(i) + RELOC_SYM));
        sites.extend((0..self.total_aux()).map(|i| self.aux_off(i) + 1));
        let rn = self.abs(BLK_REFNAME);
        sites.extend((0..self.count(BLK_REFNAME, REFNAME_SIZE)).map(|i| rn + i * REFNAME_SIZE));
        for g in 0..self.n_defined() {
            let (a0, a1) = self.aux_range(b, g);
            for a in a0..a1 {
                let ao = self.aux_off(a);
                if b[ao] != AUX_FUNCINFO {
                    continue;
                }
                let Some(fg) = self.global_of(Self::symref_at(b, ao + 1)) else { continue };
                let (d0, d1) = self.data_range(b, fg);
                if d1 < d0 + 20 {
                    continue;
                }
                let nfile = u32_at(b, d0 + 16) as usize;
                let inl_at = d0 + 20 + 4 * nfile;
                if inl_at + 4 > d1 {
                    continue;
                }
                let ninl = u32_at(b, inl_at) as usize;
                for k in 0..ninl {
                    let node = inl_at + 4 + 24 * k;
                    if node + 24 <= d1 {
                        sites.push(node + 12);
                    }
                }
            }
        }
        sites
    }

    pub(crate) fn hashed_children(&self, b: &[u8], j: usize) -> Vec<usize> {
        let (r0, r1) = self.reloc_range(b, self.hashed_global(j));
        (r0..r1)
            .map(|i| Self::symref_at(b, self.reloc_off(i) + RELOC_SYM))
            .filter(|rs| rs.pkg == PKG_IDX_HASHED && rs.idx as usize != j)
            .map(|rs| rs.idx as usize)
            .collect()
    }

    pub(crate) fn pcdata_globals(&self, b: &[u8]) -> Vec<bool> {
        let mut is = vec![false; self.n_defined()];
        for (g, p) in is.iter_mut().enumerate().skip(self.hashed_global(0)).take(self.n_hashed) {
            // pcdata syms are the only nameless hashed SRODATA syms; empty Pcsp/Pcfile/Pcline/Pcinline ones are defined but not in Aux.
            let so = self.sym_off(g);
            *p = u32_at(b, so) == 0 && b[so + 10] == SRODATA;
        }
        for g in 0..self.n_defined() {
            let (a0, a1) = self.aux_range(b, g);
            for a in a0..a1 {
                let ao = self.aux_off(a);
                if (AUX_PCSP..=AUX_PCDATA).contains(&b[ao])
                    && let Some(tg) = self.global_of(Self::symref_at(b, ao + 1))
                    && tg < is.len()
                {
                    is[tg] = true;
                }
            }
        }
        is
    }
}

pub(crate) fn path_to_prefix(s: &str) -> String {
    let slash = s.rfind('/');
    let mut out = String::with_capacity(s.len());
    for (r, c) in s.bytes().enumerate() {
        if c <= b' ' || (c == b'.' && slash.is_none_or(|i| r > i)) || c == b'%' || c == b'"' || c >= 0x7f {
            let _ = write!(out, "%{c:02x}");
        } else {
            out.push(c as char);
        }
    }
    out
}

#[expect(clippy::fn_params_excessive_bools, reason = "private helper; its one caller passes a per-symbol flag")]
fn section_byte(typ: u8, name: &[u8], is_pcdata: bool) -> u8 {
    const F_PREFIX: [&[u8]; 3] = [b"gcargs.", b"gclocals.", "gclocals·".as_bytes()];
    const F_SUFFIX: [&[u8]; 7] =
        [b".opendefer", b".arginfo0", b".arginfo1", b".argliveinfo", b".wrapinfo", b".args_stackmap", b".stkobj"];
    if typ == STEXT {
        return b't';
    }
    if typ == STEXTFIPS {
        return b'f';
    }
    if is_pcdata {
        return b'P';
    }
    if F_PREFIX.iter().any(|p| name.starts_with(p)) || F_SUFFIX.iter().any(|s| name.ends_with(s)) {
        return b'F';
    }
    if name.starts_with(b"type:") {
        return b'T';
    }
    0
}

// How a Hasher spells the package's own path inside symbol names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelfPath {
    // As the linker escapes it in symbol names (path_to_prefix).
    Escaped,
    // As written.
    Raw,
}

pub(crate) struct Hasher<'b> {
    pub obj: &'b Obj,
    pub b: &'b [u8],
    pub pkgs: Vec<&'b [u8]>,
    pub self_path: Vec<u8>,
    pub pcdata: Vec<bool>,
    memo: Vec<Option<[u8; HASH_SIZE]>>,
    pub over: Vec<Option<[u8; HASH_SIZE]>>,
}

impl<'b> Hasher<'b> {
    pub(crate) fn new(obj: &'b Obj, b: &'b [u8], self_path: &str, spelling: SelfPath) -> Hasher<'b> {
        let self_path = match spelling {
            SelfPath::Escaped => path_to_prefix(self_path),
            SelfPath::Raw => self_path.to_string(),
        };
        Hasher {
            obj,
            b,
            pkgs: obj.pkg_index_bytes(b),
            self_path: self_path.into_bytes(),
            pcdata: obj.pcdata_globals(b),
            memo: vec![None; obj.n_hashed],
            over: vec![None; obj.n_hashed],
        }
    }

    #[expect(clippy::many_single_char_names, reason = "byte-level decoder; short names follow the object file layout")]
    pub(crate) fn hash(&mut self, j: usize, depth: usize) -> Result<[u8; HASH_SIZE]> {
        if let Some(h) = self.memo[j] {
            return Ok(h);
        }
        if depth > 10000 {
            bail!("hashed symbol cycle");
        }
        let o = self.obj;
        let b = self.b;
        let g = o.hashed_global(j);
        let name = o.str_bytes(b, o.sym_off(g));
        let typ = o.sym_type(b, g);
        let mut h = Sha256::new();
        h.update([1u8]);
        let mut tmp = [0u8; 9];
        tmp[..8].copy_from_slice(&u64::from(o.sym_size(b, g)).to_le_bytes());
        tmp[8] = section_byte(typ, name, self.pcdata[g]);
        h.update(tmp);
        if typ == STEXT || typ == STEXTFIPS {
            // The Sym record already holds TrimInlineHash(s.Name).
            h.update(name);
        }
        let (d0, d1) = o.data_range(b, g);
        let mut data = &b[d0..d1];
        while let Some((&0, rest)) = data.split_last() {
            data = rest;
        }
        h.update(data);
        let (r0, r1) = o.reloc_range(b, g);
        for i in r0..r1 {
            let ro = o.reloc_off(i);
            let mut t = [0u8; 14];
            t[..4].copy_from_slice(&b[ro..ro + 4]);
            t[4] = b[ro + 4];
            t[5] = b[ro + 5];
            t[6..14].copy_from_slice(&b[ro + 7..ro + RELOC_SYM]);
            h.update(t);
            let rs = Obj::symref_at(b, ro + RELOC_SYM);
            if rs.pkg == 0 && rs.idx == 0 {
                h.update(b"nil symbol");
                continue;
            }
            if rs.pkg == PKG_IDX_HASHED && rs.idx as usize == j {
                h.update(b"self symbol");
                continue;
            }
            match rs.pkg {
                PKG_IDX_HASHED64 => {
                    ensure!((rs.idx as usize) < o.n_h64, "hashed64 index out of range");
                    h.update([0u8]);
                    let ho = o.abs(BLK_HASH64) + HASH64_SIZE * rs.idx as usize;
                    h.update(&b[ho..ho + HASH64_SIZE]);
                }
                PKG_IDX_HASHED => {
                    ensure!((rs.idx as usize) < o.n_hashed, "hashed index out of range");
                    h.update([1u8]);
                    let t = match self.over[rs.idx as usize] {
                        Some(t) => t,
                        None => self.hash(rs.idx as usize, depth + 1)?,
                    };
                    h.update(t);
                }
                PKG_IDX_NONE => {
                    ensure!((rs.idx as usize) < o.n_nonpkgdef + o.n_nonpkgref, "non-package index out of range");
                    h.update([2u8]);
                    h.update(o.str_bytes(b, o.sym_off(o.nonpkg_global(rs.idx as usize))));
                }
                PKG_IDX_BUILTIN => {
                    h.update([3u8]);
                    h.update(rs.idx.to_le_bytes());
                }
                PKG_IDX_SELF => {
                    h.update(&self.self_path);
                    h.update(rs.idx.to_le_bytes());
                }
                p => {
                    let path = self.pkgs.get(p as usize).context("bad pkg index")?;
                    h.update(path);
                    h.update(rs.idx.to_le_bytes());
                }
            }
        }
        let full: [u8; 32] = h.finalize().into();
        let mut out = [0u8; HASH_SIZE];
        out.copy_from_slice(&full[..HASH_SIZE]);
        self.memo[j] = Some(out);
        Ok(out)
    }

    pub(crate) fn stored(&self, j: usize) -> [u8; HASH_SIZE] {
        let ho = self.obj.hash_off(j);
        self.b[ho..ho + HASH_SIZE].try_into().expect("a HASH_SIZE slice converts to an array")
    }
}
