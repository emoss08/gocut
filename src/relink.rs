//! Rebuilds a package's export data after the import graph of one of its dependencies changed.
//!
//! Export data lists, for every package it mentions, that package's imports, and so on down: the
//! whole transitive import graph. Adding an import to one package therefore changes the export data
//! of every package above it, although the compiler reads those lists only to copy them into the
//! next package's export data. Nothing else in a compiled package depends on them.
//!
//! The compiler's export writer (cmd/compile/internal/noder, `writeUnifiedExport` and the linker
//! behind it) numbers elements in the order a depth-first walk first reaches them: the package's
//! own Pkg element and everything under it, then each object, then the private root's strings.
//! Strings are shared by value and packages by path. `relink` repeats that walk over the old export
//! data with the new import lists in place, which yields the new String and Pkg sections and the
//! new index of every string and package; every other element keeps its index and its payload, and
//! only its reference table is renumbered.
//!
//! The new lists come from the dependencies whose import graph changed. Each one's export data
//! describes every package below it, so the walk follows a changed dependency through that
//! dependency's own export data and everything else through the old export data. A package
//! whose imports changed changes the graph of everything above it, so whatever the walk reaches
//! below an unchanged dependency is unchanged too.
//!
//! One package sits outside the import graph: `builtin`, which objects such as `error` refer to
//! and nothing imports. The walk first reaches it from an object, and the compiler then copies its
//! element from whichever import supplied that object, adding one reference per copy. Which import
//! that is depends on the order the sources import them, so `relink` keeps the element the old
//! export data has, as the rest of gocut's patching does; a change there is one the verified sample
//! would report as a mismatch in bytes that no reader interprets.
//!
//! The walk is checked against the compiler before it is trusted: run with the old import lists it
//! must arrive at the String and Pkg sections the old export data has, byte for byte, and reach
//! every other element in index order. `relink` refuses otherwise.

use anyhow::{Context, Result, bail, ensure};

use crate::export::Export;
use crate::fx::FxHashMap;
use crate::pkgbits::{self, NUM_SECTIONS, Reloc, Section, encode_elem, put_uvarint, read_relocs};

pub(crate) struct Relinked {
    pub bits: Vec<u8>,
    /// Whether every package kept its name. Object hashes cover a package's path and name, so they
    /// are unchanged exactly when this holds.
    pub names_kept: bool,
}

const UNSET: u32 = u32::MAX;

// What the walk reaches next.
#[derive(Clone, Copy)]
enum Target<'x> {
    // A string of the old export data, by index.
    OldStr(u32),
    // A string by value: a package's path or name.
    Str(&'x [u8]),
    // A package an element of the old export data refers to, by its old index.
    OldPkg(u32),
    // A package in an import list: Pkg element `.1` of source `.0`.
    PkgAt(u32, u32),
    Elem(Section, u32),
}

// A Pkg element being written: its reference table as (kind, string or path) and its payload. The
// indices are filled in when the walk has reached everything the element refers to.
struct PkgOut<'x> {
    slot: usize,
    refs: Vec<(Section, &'x [u8])>,
    payload: Vec<u8>,
}

// The targets of one element, as a range of the walk's target stack.
struct Frame<'x> {
    start: usize,
    next: usize,
    pkg: Option<PkgOut<'x>>,
}

// Export data the walk reads import lists from: the old export data (source 0), then each
// dependency whose import graph changed. `paths` remembers the path of each Pkg element read.
struct Source<'x> {
    ex: &'x Export<'x>,
    paths: Vec<Option<&'x [u8]>>,
}

struct Walk<'x> {
    ex: &'x Export<'x>,
    self_path: &'x [u8],
    srcs: Vec<Source<'x>>,
    // Each dependency whose import graph changed: its path, its source and its own Pkg element there.
    changed: FxHashMap<&'x [u8], (u32, u32)>,
    // The old index of each package of the old export data, when a changed dependency is read.
    old_by_path: FxHashMap<&'x [u8], u32>,
    strings: Vec<&'x [u8]>,
    by_content: FxHashMap<&'x [u8], u32>,
    // The new index of each old string and package, UNSET until the walk reaches it.
    old_strings: Vec<u32>,
    old_pkgs: Vec<u32>,
    pkgs: Vec<Vec<u8>>,
    pkg_ids: FxHashMap<&'x [u8], u32>,
    // Elements of the other sections reached so far, by section. They must be reached in index order.
    reached: [u32; NUM_SECTIONS],
    names_kept: bool,
    // Packages first reached from an object rather than through an import list.
    strays: Vec<&'x [u8]>,
    targets: Vec<Target<'x>>,
    relocs: Vec<Reloc>,
    // The packages in the import list being written, with their place in its reference table.
    imported: FxHashMap<&'x [u8], u64>,
}

const SECTIONS: [Section; NUM_SECTIONS] = [
    Section::String,
    Section::Meta,
    Section::PosBase,
    Section::Pkg,
    Section::Name,
    Section::Type,
    Section::Obj,
    Section::ObjExt,
    Section::ObjDict,
    Section::Body,
];

fn lossy(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

impl<'x> Walk<'x> {
    fn new(ex: &'x Export<'x>, deps: &[&'x Export<'x>]) -> Result<Walk<'x>> {
        let pb = &ex.pb;
        let mut w = Walk {
            ex,
            self_path: ex.self_path.as_bytes(),
            srcs: Vec::with_capacity(deps.len() + 1),
            changed: FxHashMap::default(),
            old_by_path: FxHashMap::default(),
            strings: Vec::with_capacity(pb.num(Section::String)),
            by_content: FxHashMap::default(),
            old_strings: vec![UNSET; pb.num(Section::String)],
            old_pkgs: vec![UNSET; pb.num(Section::Pkg)],
            pkgs: Vec::with_capacity(pb.num(Section::Pkg)),
            pkg_ids: FxHashMap::default(),
            reached: [0; NUM_SECTIONS],
            names_kept: true,
            strays: Vec::new(),
            targets: Vec::new(),
            relocs: Vec::new(),
            imported: FxHashMap::default(),
        };
        for e in std::iter::once(ex).chain(deps.iter().copied()) {
            w.srcs.push(Source { ex: e, paths: vec![None; e.pb.num(Section::Pkg)] });
        }
        ensure!(w.path_of(0, ex.self_pkg())? == w.self_path, "export data is not for {}", ex.self_path);
        if !deps.is_empty() {
            for i in 0..u32::try_from(pb.num(Section::Pkg))? {
                let path = w.path_of(0, i)?;
                ensure!(w.old_by_path.insert(path, i).is_none(), "export data describes {} twice", lossy(path));
            }
        }
        for (src, d) in (1u32..).zip(deps) {
            w.changed.insert(d.self_path.as_bytes(), (src, d.self_pkg()));
        }
        Ok(w)
    }

    fn path_of(&mut self, src: u32, idx: u32) -> Result<&'x [u8]> {
        let s = &mut self.srcs[src as usize];
        let slot = s.paths.get_mut(idx as usize).context("package index out of range")?;
        if let Some(p) = *slot {
            return Ok(p);
        }
        let raw = s.ex.pkg_path(idx)?;
        let path = if raw.is_empty() { s.ex.self_path.as_bytes() } else { raw };
        *slot = Some(path);
        Ok(path)
    }

    fn intern(&mut self, s: &'x [u8]) -> Result<u32> {
        if let Some(&id) = self.by_content.get(s) {
            return Ok(id);
        }
        let id = u32::try_from(self.strings.len())?;
        self.by_content.insert(s, id);
        self.strings.push(s);
        Ok(id)
    }

    fn intern_old(&mut self, idx: u32) -> Result<()> {
        let slot = *self.old_strings.get(idx as usize).context("string index out of range")?;
        if slot == UNSET {
            self.old_strings[idx as usize] = self.intern(self.ex.pb.string(idx)?)?;
        }
        Ok(())
    }

    // A package in an import list. pkgWriter.pkgIdx writes the path ("" for the package itself),
    // the name and the imports; linker.relocPkg then appends a reference to the real path and
    // points the payload at it.
    fn enter_pkg_at(&mut self, src: u32, idx: u32) -> Result<Option<Frame<'x>>> {
        let path = self.path_of(src, idx)?;
        // A dependency whose import graph changed is read from its own export data.
        let (from, at) = match self.changed.get(path) {
            Some(&own) if src == 0 => own,
            _ => (src, idx),
        };
        if let Some(&id) = self.pkg_ids.get(path) {
            if src == 0 {
                self.old_pkgs[idx as usize] = id;
            }
            return Ok(None);
        }
        let e = self.srcs[from as usize].ex.pkg_elem(at)?;
        if from != 0
            && let Some(&old) = self.old_by_path.get(path)
            && self.ex.pkg_elem(old)?.name != e.name
        {
            self.names_kept = false;
        }
        let slot = self.pkgs.len();
        let id = u32::try_from(slot)?;
        self.pkg_ids.insert(path, id);
        if src == 0 {
            self.old_pkgs[idx as usize] = id;
        }
        self.pkgs.push(Vec::new());

        let start = self.targets.len();
        let written: &'x [u8] = if path == self.self_path { b"" } else { path };
        let mut refs = vec![(Section::String, written)];
        self.targets.push(Target::Str(written));
        let mut tail = Vec::new();
        // pkgbits.Encoder keeps one entry per distinct reference: a name spelled like the path
        // shares the path's entry, and so would a package listed twice.
        if let Some(name) = e.name {
            if name == written {
                put_uvarint(&mut tail, 0);
            } else {
                put_uvarint(&mut tail, refs.len() as u64);
                refs.push((Section::String, name));
                self.targets.push(Target::Str(name));
            }
            put_uvarint(&mut tail, e.imports.len() as u64);
            self.imported.clear();
            for &imp in &e.imports {
                let p = self.path_of(from, imp)?;
                let next = refs.len() as u64;
                let i = *self.imported.entry(p).or_insert(next);
                if i == next {
                    refs.push((Section::Pkg, p));
                    self.targets.push(Target::PkgAt(from, imp));
                }
                put_uvarint(&mut tail, i);
            }
        }
        let mut payload = Vec::with_capacity(tail.len() + 2);
        put_uvarint(&mut payload, refs.len() as u64);
        payload.extend_from_slice(&tail);
        refs.push((Section::String, path));
        self.targets.push(Target::Str(path));
        Ok(Some(Frame { start, next: start, pkg: Some(PkgOut { slot, refs, payload }) }))
    }

    // A package an object refers to. One the import lists did not reach is outside the import
    // graph and keeps the element the old export data has for it. Only a package written by path
    // alone is expected here; one with imports of its own would have been numbered with the graph.
    fn enter_old_pkg(&mut self, idx: u32) -> Result<Option<Frame<'x>>> {
        if *self.old_pkgs.get(idx as usize).context("package index out of range")? != UNSET {
            return Ok(None);
        }
        let path = self.path_of(0, idx)?;
        if let Some(&id) = self.pkg_ids.get(path) {
            self.old_pkgs[idx as usize] = id;
            return Ok(None);
        }
        let raw = self.ex.pb.raw(Section::Pkg, idx)?;
        let at = read_relocs(raw, &mut self.relocs)?;
        let start = self.targets.len();
        let mut refs = Vec::with_capacity(self.relocs.len());
        for r in &self.relocs {
            ensure!(r.kind == Section::String, "package {} outside the import graph has imports", lossy(path));
            let s = self.ex.pb.string(r.idx)?;
            refs.push((Section::String, s));
            self.targets.push(Target::Str(s));
        }
        let slot = self.pkgs.len();
        let id = u32::try_from(slot)?;
        self.pkg_ids.insert(path, id);
        self.old_pkgs[idx as usize] = id;
        self.pkgs.push(Vec::new());
        self.strays.push(path);
        Ok(Some(Frame { start, next: start, pkg: Some(PkgOut { slot, refs, payload: raw[at..].to_vec() }) }))
    }

    fn finish_pkg(&mut self, out: &PkgOut<'x>) -> Result<()> {
        let relocs = out
            .refs
            .iter()
            .map(|&(kind, v)| {
                let ids = if kind == Section::Pkg { &self.pkg_ids } else { &self.by_content };
                Ok(Reloc { kind, idx: *ids.get(v).context("package element refers to something unreached")? })
            })
            .collect::<Result<Vec<_>>>()?;
        encode_elem(&relocs, &out.payload, &mut self.pkgs[out.slot]);
        Ok(())
    }

    // Adds what element `k`:`idx` of the old export data refers to, in order, to the target stack.
    fn push_targets(&mut self, k: Section, idx: u32) -> Result<()> {
        read_relocs(self.ex.pb.raw(k, idx)?, &mut self.relocs)?;
        self.targets.extend(self.relocs.iter().map(|r| match r.kind {
            Section::String => Target::OldStr(r.idx),
            Section::Pkg => Target::OldPkg(r.idx),
            kind => Target::Elem(kind, r.idx),
        }));
        Ok(())
    }

    // The next element of a section must be the one the walk has just reached.
    fn reach(&mut self, k: Section, idx: u32) -> Result<bool> {
        let n = &mut self.reached[k as usize];
        if idx < *n {
            return Ok(false);
        }
        ensure!(idx == *n, "{k:?} element {idx} reached out of order");
        ensure!((idx as usize) < self.ex.pb.num(k), "{k:?}:{idx} out of bounds");
        *n += 1;
        Ok(true)
    }

    // linker.relocObj takes the next index in four sections at once, copies the Obj, Name and
    // ObjDict elements, then the ObjExt element, then the object's inline bodies.
    fn enter_elem(&mut self, k: Section, idx: u32) -> Result<Option<Frame<'x>>> {
        let start = self.targets.len();
        match k {
            Section::Obj => {
                if !self.reach(Section::Obj, idx)? {
                    return Ok(None);
                }
                for part in [Section::Obj, Section::Name, Section::ObjDict, Section::ObjExt] {
                    if part != Section::Obj {
                        ensure!(self.reach(part, idx)?, "{part:?} element {idx} reached before its object");
                    }
                    self.push_targets(part, idx)?;
                }
                let bodies = self.targets.len();
                self.targets.extend(self.ex.bodies_of(idx).iter().map(|&b| Target::Elem(Section::Body, b)));
                self.targets[bodies..].sort_unstable_by_key(|t| match t {
                    Target::Elem(_, b) => *b,
                    _ => 0,
                });
            }
            Section::Name | Section::ObjExt | Section::ObjDict => {
                bail!("{k:?} element {idx} referenced directly");
            }
            Section::String | Section::Pkg => bail!("{k:?} is not walked as an element"),
            Section::Meta | Section::PosBase | Section::Type | Section::Body => {
                if !self.reach(k, idx)? {
                    return Ok(None);
                }
                self.push_targets(k, idx)?;
            }
        }
        Ok(Some(Frame { start, next: start, pkg: None }))
    }

    fn run(&mut self) -> Result<()> {
        let pb = &self.ex.pb;
        ensure!(pb.num(Section::Meta) == 2, "export data has {} roots, want 2", pb.num(Section::Meta));
        // The two roots are created before anything is walked.
        self.reached[Section::Meta as usize] = 2;
        // writeUnifiedExport: the package's own Pkg element, then every object in index order (an
        // object first reached under an earlier one is already numbered), then the private root's
        // strings.
        self.targets.push(Target::PkgAt(0, self.ex.self_pkg()));
        self.targets.extend((0..u32::try_from(pb.num(Section::Obj))?).map(|i| Target::Elem(Section::Obj, i)));
        self.push_targets(Section::Meta, 1)?;
        let mut stack = vec![Frame { start: 0, next: 0, pkg: None }];
        while let Some(top) = stack.last_mut() {
            let Some(&t) = self.targets.get(top.next) else {
                if let Some(f) = stack.pop() {
                    self.targets.truncate(f.start);
                    if let Some(out) = &f.pkg {
                        self.finish_pkg(out)?;
                    }
                }
                continue;
            };
            top.next += 1;
            let frame = match t {
                Target::OldStr(i) => {
                    self.intern_old(i)?;
                    None
                }
                Target::Str(s) => {
                    self.intern(s)?;
                    None
                }
                Target::OldPkg(i) => self.enter_old_pkg(i)?,
                Target::PkgAt(src, i) => self.enter_pkg_at(src, i)?,
                Target::Elem(k, idx) => self.enter_elem(k, idx)?,
            };
            stack.extend(frame);
        }
        for k in SECTIONS {
            if k != Section::String && k != Section::Pkg {
                ensure!(self.reached[k as usize] as usize == pb.num(k), "{k:?} has elements the walk did not reach");
            }
        }
        Ok(())
    }

    // That the walk arrived at the String and Pkg sections the export data already has.
    fn check_same(&self) -> Result<()> {
        let pb = &self.ex.pb;
        for (i, s) in self.strings.iter().enumerate() {
            let old = pb.string(u32::try_from(i)?).ok();
            ensure!(old == Some(*s), "string {i} is {:?}, the walk reaches {:?} there", old.map(lossy), lossy(s));
        }
        ensure!(self.strings.len() == pb.num(Section::String), "the walk does not reach every string");
        ensure!(self.pkgs.len() == pb.num(Section::Pkg), "the walk does not reach every package");
        for (i, p) in self.pkgs.iter().enumerate() {
            ensure!(pb.raw(Section::Pkg, u32::try_from(i)?)? == p.as_slice(), "Pkg element {i} is written differently");
        }
        Ok(())
    }

    fn renumber(&self, r: Reloc) -> Result<Reloc> {
        let idx = match r.kind {
            Section::String => *self.old_strings.get(r.idx as usize).context("string index out of range")?,
            Section::Pkg => *self.old_pkgs.get(r.idx as usize).context("package index out of range")?,
            _ => return Ok(r),
        };
        ensure!(idx != UNSET, "{:?} {} is referenced but was not reached", r.kind, r.idx);
        Ok(Reloc { kind: r.kind, idx })
    }

    // Every element in file order: the new strings and packages, and each other element with its
    // references renumbered.
    fn emit(&self, replaced: &FxHashMap<usize, Vec<u8>>) -> Result<Vec<u8>> {
        let pb = &self.ex.pb;
        let mut ends_ends = [0u32; NUM_SECTIONS];
        let mut ends: Vec<u32> = Vec::with_capacity(pb.elem_ends.len() + self.strings.len());
        let mut data: Vec<u8> = Vec::with_capacity(pb.data.len() + (pb.data.len() >> 4) + 4096);
        let end = |data: &Vec<u8>| u32::try_from(data.len()).context("export data longer than u32");
        let (mut old, mut new, mut table) = (Vec::new(), Vec::new(), Vec::new());
        for k in SECTIONS {
            match k {
                Section::String => {
                    for s in &self.strings {
                        data.extend_from_slice(s);
                        ends.push(end(&data)?);
                    }
                }
                Section::Pkg => {
                    for p in &self.pkgs {
                        data.extend_from_slice(p);
                        ends.push(end(&data)?);
                    }
                }
                _ => {
                    for idx in 0..u32::try_from(pb.num(k))? {
                        let a = pb.abs(k, idx)?;
                        let raw = replaced.get(&a).map_or_else(|| pb.raw_abs(a), Vec::as_slice);
                        let at = read_relocs(raw, &mut old)?;
                        new.clear();
                        for &r in &old {
                            new.push(self.renumber(r)?);
                        }
                        if new == old {
                            data.extend_from_slice(raw);
                        } else {
                            // What is written must be what the compiler would write: its table
                            // is the shortest encoding, so the old one must be too.
                            table.clear();
                            encode_elem(&old, &[], &mut table);
                            ensure!(table == raw[..at], "{k:?} element {idx} has an unusual reference table");
                            encode_elem(&new, &raw[at..], &mut data);
                        }
                        ends.push(end(&data)?);
                    }
                }
            }
            ends_ends[k as usize] = u32::try_from(ends.len())?;
        }
        Ok(pkgbits::dump_flat(pb.version, pb.flags, &ends_ends, &ends, &data))
    }
}

fn walk<'x>(ex: &'x Export<'x>, deps: &[&'x Export<'x>]) -> Result<Walk<'x>> {
    let mut w = Walk::new(ex, deps)?;
    w.run()?;
    Ok(w)
}

/// Checks that the walk, given the import lists `bits` already holds, writes `bits` back byte for
/// byte. This is what ties the walk to the compiler that wrote `bits`.
pub(crate) fn reproduce<'x>(ex: &'x Export<'x>, bits: &[u8]) -> Result<()> {
    let w = walk(ex, &[])?;
    w.check_same()?;
    ensure!(w.emit(&FxHashMap::default())? == bits, "the rebuilt export data differs");
    Ok(())
}

/// Rebuilds the export data `ex` was parsed from, reading the import lists of each dependency in
/// `deps` (the ones whose import graph changed) from that dependency's own export data, and
/// putting each element in `replaced` (by absolute index) in the place of the element there. A
/// replacement keeps the element's reference table.
pub(crate) fn relink<'x>(
    ex: &'x Export<'x>,
    deps: &[&'x Export<'x>],
    replaced: &FxHashMap<usize, Vec<u8>>,
) -> Result<Relinked> {
    let before = walk(ex, &[]).and_then(|w| w.check_same().map(|()| w));
    let before = before.context("cannot reproduce the export data's layout")?;
    let w = walk(ex, deps)?;
    // A package that was in the import graph and no longer is would be copied from an import's
    // export data, in a form the old export data does not show.
    ensure!(
        w.strays.iter().all(|p| before.strays.contains(p)),
        "a package left the import graph but is still referenced"
    );
    Ok(Relinked { bits: w.emit(replaced)?, names_kept: w.names_kept })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::Inliner;

    const SELF: &str = "ex/p";

    fn uv(xs: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        for &x in xs {
            put_uvarint(&mut out, x);
        }
        out
    }

    fn el(relocs: &[(Section, u32)], payload: &[u8]) -> Vec<u8> {
        let relocs: Vec<Reloc> = relocs.iter().map(|&(kind, idx)| Reloc { kind, idx }).collect();
        let mut out = Vec::new();
        encode_elem(&relocs, payload, &mut out);
        out
    }

    fn dump(sections: [Vec<Vec<u8>>; NUM_SECTIONS]) -> Vec<u8> {
        let mut ends = [0u32; NUM_SECTIONS];
        let mut sum = 0;
        for (e, sec) in ends.iter_mut().zip(&sections) {
            sum += u32::try_from(sec.len()).unwrap();
            *e = sum;
        }
        let elems: Vec<Vec<u8>> = sections.into_iter().flatten().collect();
        pkgbits::dump(pkgbits::VERSION_PRESERVE_METHOD_ORDER, 0, &ends, &elems).unwrap()
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Imports {
        // ex/p imports ex/a, which imports nothing.
        Flat,
        // ex/a imports ex/z and ex/y, and ex/z imports ex/y.
        Deep,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Order {
        Compiler,
        // The last two strings swapped: valid export data, but not numbered as the compiler would.
        Other,
    }

    // Export data for ex/p, numbered the way the compiler numbers it: the package graph and its
    // strings, then var V (its position base, its type T from ex/a, then its name).
    fn build_in(imports: Imports, order: Order) -> Vec<u8> {
        let (s, p) = (Section::String, Section::Pkg);
        let mut strings: Vec<&[u8]> = vec![b"", b"p", b"ex/a", b"a"];
        let mut pkgs = vec![];
        if imports == Imports::Deep {
            strings.extend([b"ex/z".as_slice(), b"z", b"ex/y", b"y"]);
            pkgs.push(el(&[(s, 0), (s, 1), (p, 1), (s, 8)], &uv(&[3, 1, 1, 2])));
            pkgs.push(el(&[(s, 2), (s, 3), (p, 2), (p, 3), (s, 2)], &uv(&[4, 1, 2, 2, 3])));
            pkgs.push(el(&[(s, 4), (s, 5), (p, 3), (s, 4)], &uv(&[3, 1, 1, 2])));
            pkgs.push(el(&[(s, 6), (s, 7), (s, 6)], &uv(&[2, 1, 0])));
        } else {
            pkgs.push(el(&[(s, 0), (s, 1), (p, 1), (s, 4)], &uv(&[3, 1, 1, 2])));
            pkgs.push(el(&[(s, 2), (s, 3), (s, 2)], &uv(&[2, 1, 0])));
        }
        let base = u32::try_from(strings.len()).unwrap();
        let (t, v) = if order == Order::Compiler { (base + 2, base + 3) } else { (base + 3, base + 2) };
        strings.extend([b"ex/p".as_slice(), b"f.go"]);
        strings.extend(if order == Order::Compiler { [b"T".as_slice(), b"V"] } else { [b"V".as_slice(), b"T"] });
        dump([
            strings.iter().map(|x| x.to_vec()).collect(),
            vec![el(&[(p, 0), (Section::Obj, 0)], &[0, 0, 1, 0, 1, 0]), el(&[], &[0, 0])],
            vec![el(&[(s, base + 1)], &[0, 1])],
            pkgs,
            vec![el(&[(p, 0), (s, v)], &[0, 1, 4])],
            vec![el(&[(p, 1), (s, t)], &[8, 0])],
            vec![el(&[(Section::PosBase, 0), (Section::Type, 0)], &[1, 0, 9, 2, 0, 1])],
            vec![el(&[], &[0])],
            vec![el(&[], &[0, 0, 0])],
            vec![],
        ])
    }

    fn build(imports: Imports) -> Vec<u8> {
        build_in(imports, Order::Compiler)
    }

    // Export data for a dependency that exports nothing: the packages it describes, itself
    // first, each as (path, name, indices of the packages it imports).
    fn dep(pkgs: &[(&str, &str, &[u32])]) -> Vec<u8> {
        let s = Section::String;
        let mut strings = Vec::new();
        let mut elems = Vec::new();
        for &(path, name, imports) in pkgs {
            let at = u32::try_from(strings.len()).unwrap();
            strings.extend([path.as_bytes().to_vec(), name.as_bytes().to_vec()]);
            let mut relocs = vec![(s, at), (s, at + 1)];
            relocs.extend(imports.iter().map(|&i| (Section::Pkg, i)));
            relocs.push((s, at));
            let last = relocs.len() as u64 - 1;
            let mut payload = vec![last, 1, imports.len() as u64];
            payload.extend((2..).take(imports.len()));
            elems.push(el(&relocs, &uv(&payload)));
        }
        dump([
            strings,
            vec![el(&[(Section::Pkg, 0)], &[0, 0, 0]), el(&[], &[0, 0])],
            vec![],
            elems,
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        ])
    }

    fn deep_a() -> Vec<u8> {
        dep(&[("ex/a", "a", &[1, 2]), ("ex/z", "z", &[2]), ("ex/y", "y", &[])])
    }

    fn pkg_names(bits: &[u8]) -> Vec<(String, String)> {
        let ex = Export::parse(bits, SELF, Inliner::Classic).unwrap();
        (0..u32::try_from(ex.pb.num(Section::Pkg)).unwrap())
            .map(|i| {
                let e = ex.pkg_elem(i).unwrap();
                (lossy(e.path).into_owned(), lossy(e.name.unwrap_or_default()).into_owned())
            })
            .collect()
    }

    #[test]
    fn reproduces_the_layout_it_was_given() {
        for imports in [Imports::Flat, Imports::Deep] {
            let bits = build(imports);
            let ex = Export::parse(&bits, SELF, Inliner::Classic).unwrap();
            reproduce(&ex, &bits).unwrap();
            let out = relink(&ex, &[], &FxHashMap::default()).unwrap();
            assert!(out.bits == bits, "relinking with nothing changed gives the same bytes");
            assert!(out.names_kept, "no package was renamed");
        }
    }

    #[test]
    fn a_dependency_gaining_imports_gives_the_layout_the_compiler_writes() {
        let (flat, deep, a) = (build(Imports::Flat), build(Imports::Deep), deep_a());
        let ex = Export::parse(&flat, SELF, Inliner::Classic).unwrap();
        let a = Export::parse(&a, "ex/a", Inliner::Classic).unwrap();
        let out = relink(&ex, &[&a], &FxHashMap::default()).unwrap();
        assert!(out.bits == deep, "the relinked export data matches a fresh one");
        assert!(out.names_kept, "no package was renamed");
    }

    #[test]
    fn a_dependency_losing_imports_drops_their_packages_and_strings() {
        let (flat, deep, a) = (build(Imports::Flat), build(Imports::Deep), dep(&[("ex/a", "a", &[])]));
        let ex = Export::parse(&deep, SELF, Inliner::Classic).unwrap();
        let a = Export::parse(&a, "ex/a", Inliner::Classic).unwrap();
        let out = relink(&ex, &[&a], &FxHashMap::default()).unwrap();
        assert!(out.bits == flat, "the relinked export data matches a fresh one");
    }

    #[test]
    fn a_renamed_package_is_reported() {
        let bits = build(Imports::Deep);
        let a = dep(&[("ex/a", "a", &[1, 2]), ("ex/z", "z", &[2]), ("ex/y", "why", &[])]);
        let ex = Export::parse(&bits, SELF, Inliner::Classic).unwrap();
        let a = Export::parse(&a, "ex/a", Inliner::Classic).unwrap();
        let out = relink(&ex, &[&a], &FxHashMap::default()).unwrap();
        assert!(!out.names_kept, "ex/y changed its package name");
        assert!(pkg_names(&out.bits).contains(&("ex/y".into(), "why".into())), "the new name is written");
    }

    #[test]
    fn a_replaced_element_is_written_with_renumbered_references() {
        let (flat, a) = (build(Imports::Flat), deep_a());
        let ex = Export::parse(&flat, SELF, Inliner::Classic).unwrap();
        let a = Export::parse(&a, "ex/a", Inliner::Classic).unwrap();
        // V's Obj element with another line number, as a position-only patch would write it.
        let v = ex.find(SELF, "V").unwrap();
        let abs = ex.pb.abs(Section::Obj, v).unwrap();
        let moved = el(&[(Section::PosBase, 0), (Section::Type, 0)], &[1, 0, 42, 2, 0, 1]);
        let out = relink(&ex, &[&a], &FxHashMap::from_iter([(abs, moved.clone())])).unwrap();
        let got = Export::parse(&out.bits, SELF, Inliner::Classic).unwrap();
        assert_eq!(got.pb.raw(Section::Obj, v).unwrap(), moved.as_slice(), "the replacement is in place");
        assert_eq!(got.pb.num(Section::Pkg), 4, "ex/p, ex/a, ex/z and ex/y");
        reproduce(&got, &out.bits).unwrap();
    }

    #[test]
    fn a_layout_the_walk_cannot_reproduce_is_refused() {
        let bits = build_in(Imports::Deep, Order::Other);
        let ex = Export::parse(&bits, SELF, Inliner::Classic).unwrap();
        assert!(reproduce(&ex, &bits).is_err(), "the strings are not in the compiler's order");
        assert!(relink(&ex, &[], &FxHashMap::default()).is_err(), "so nothing is rebuilt from it");
    }

    // What lets a package be reused across an import change: nothing it uses hashes differently,
    // and only the graph hash says the export data must be rebuilt.
    #[test]
    fn an_import_change_moves_the_graph_hash_and_no_object_hash() {
        let (flat, deep) = (build(Imports::Flat), build(Imports::Deep));
        let a = Export::parse(&flat, SELF, Inliner::Classic).unwrap();
        let b = Export::parse(&deep, SELF, Inliner::Classic).unwrap();
        let v = a.find(SELF, "V").unwrap();
        assert_eq!(b.find(SELF, "V"), Some(v), "V has the same index in both");
        assert!(a.obj_hash(v).unwrap() == b.obj_hash(v).unwrap(), "V reaches ex/a, whose imports differ");
        assert!(a.pkg_hash().unwrap() != b.pkg_hash().unwrap(), "the import graphs differ");
    }

    #[test]
    fn a_renamed_package_moves_the_hash_of_what_refers_to_it() {
        let bits = build(Imports::Flat);
        let other = dep(&[("ex/a", "other", &[])]);
        let ex = Export::parse(&bits, SELF, Inliner::Classic).unwrap();
        let a = Export::parse(&other, "ex/a", Inliner::Classic).unwrap();
        let out = relink(&ex, &[&a], &FxHashMap::default()).unwrap();
        let renamed = Export::parse(&out.bits, SELF, Inliner::Classic).unwrap();
        let v = ex.find(SELF, "V").unwrap();
        assert!(ex.obj_hash(v).unwrap() != renamed.obj_hash(v).unwrap(), "V's type is from the renamed package");
    }
}
