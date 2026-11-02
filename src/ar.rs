use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, ensure};

const MAGIC: &[u8] = b"!<arch>\n";
const HEADER_LEN: usize = 60;
const NAME: Range<usize> = 0..16;
const SIZE: Range<usize> = 48..58;
const FIRST_DATA: usize = MAGIC.len() + HEADER_LEN;
const PKGDEF: &str = "__.PKGDEF";
const EXPORT_TRAILER: &[u8] = b"\n$$\n";

pub struct Member {
    pub name: String,
    pub header: Vec<u8>,
    pub data: Vec<u8>,
}

fn header(h: &[u8]) -> Result<(String, usize)> {
    let name = String::from_utf8_lossy(&h[NAME]).trim().to_string();
    let size = std::str::from_utf8(&h[SIZE])?.trim().parse()?;
    Ok((name, size))
}

pub fn parse(b: &[u8]) -> Result<Vec<Member>> {
    ensure!(b.starts_with(MAGIC), "not an archive");
    let mut i = MAGIC.len();
    let mut out = Vec::new();
    while i < b.len() {
        let h = b.get(i..i + HEADER_LEN).context("truncated archive header")?;
        let (name, size) = header(h)?;
        let start = i + HEADER_LEN;
        let data = b.get(start..start + size).context("truncated archive member")?.to_vec();
        out.push(Member { name, header: h.to_vec(), data });
        i = start + size + (size & 1);
    }
    Ok(out)
}

pub fn write(members: &[Member]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for m in members {
        let mut h = m.header.clone();
        let size = format!("{:<10}", m.data.len());
        h[SIZE].copy_from_slice(size.as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(&m.data);
        if m.data.len() & 1 == 1 {
            out.push(0);
        }
    }
    out
}

pub fn member_data<'b>(b: &'b [u8], want: &str) -> Option<&'b [u8]> {
    let mut i = MAGIC.len();
    if !b.starts_with(MAGIC) {
        return None;
    }
    while i < b.len() {
        let (name, size) = header(b.get(i..i + HEADER_LEN)?).ok()?;
        let start = i + HEADER_LEN;
        if name == want {
            return b.get(start..start + size);
        }
        i = start + size + (size & 1);
    }
    None
}

pub fn member<'m>(ms: &'m [Member], name: &str) -> Option<&'m Member> {
    ms.iter().find(|m| m.name == name)
}

fn open_pkgdef(path: &Path) -> Result<(File, usize)> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut head = [0u8; FIRST_DATA];
    f.read_exact(&mut head)?;
    ensure!(head.starts_with(MAGIC), "not an archive");
    let (name, size) = header(&head[MAGIC.len()..])?;
    ensure!(name == PKGDEF, "first member is not __.PKGDEF");
    Ok((f, size))
}

pub fn read_pkgdef(path: &Path) -> Result<Vec<u8>> {
    let (mut f, size) = open_pkgdef(path)?;
    let mut data = vec![0u8; size];
    f.read_exact(&mut data)?;
    Ok(data)
}

pub fn read_pkgdef_fingerprint(path: &Path) -> Result<[u8; 8]> {
    let (mut f, size) = open_pkgdef(path)?;
    ensure!(size >= 12, "export data too short");
    f.seek(SeekFrom::Start((FIRST_DATA + size - 12) as u64))?;
    let mut tail = [0u8; 12];
    f.read_exact(&mut tail)?;
    ensure!(&tail[8..] == EXPORT_TRAILER, "export data missing trailer");
    let mut fp = [0u8; 8];
    fp.copy_from_slice(&tail[..8]);
    Ok(fp)
}
