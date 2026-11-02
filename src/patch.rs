use std::collections::{HashMap, HashSet};

use crate::fx::{FxHashMap, FxHashSet};
use std::fmt;

use anyhow::{Context, Result, ensure};

use crate::ar;
use crate::export::{self, Export, Inliner};
use crate::goobj::{HASH_SIZE, Hasher, Obj, SYMREF_SIZE, SelfPath, SymRef};
use crate::pkgbits::{self, Section};
use crate::store::{LsymTable, timed};

#[derive(Debug)]
pub(crate) struct Guard(pub String);

impl fmt::Display for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Guard {}

pub(crate) struct ChangedDep<'a> {
    /// None when the caller knows this package copies nothing from the dependency and the new
    /// export data hashes like the old (see `patch_pkgdef` and the full-DWARF guard).
    pub old: Option<&'a Export<'a>>,
    pub new: &'a Export<'a>,
    pub old_lsyms: &'a LsymTable,
    pub new_lsyms: &'a LsymTable,
}

pub(crate) fn lsym_table(ex: &Export<'_>) -> LsymTable {
    ex.idents
        .iter()
        .enumerate()
        .filter_map(|(i, id)| {
            let v = ex.lsyms(u32::try_from(i).ok()?).ok()?;
            Some((id.path.clone(), id.name.clone(), v))
        })
        .collect()
}

#[derive(Default)]
struct IndexMaps<'a> {
    old_to_ident: FxHashMap<(&'a str, i64), (&'a str, usize)>,
    new_vals: FxHashMap<(&'a str, &'a str), &'a [i64]>,
    moved_paths: FxHashSet<&'a str>,
}

fn index_maps<'a>(deps: &[ChangedDep<'a>]) -> Result<IndexMaps<'a>> {
    let mut m = IndexMaps::default();
    let mut old_vals: FxHashMap<(&'a str, &'a str), &'a [i64]> = FxHashMap::default();
    for d in deps {
        for (path, name, vals) in d.old_lsyms {
            for (s, &v) in vals.iter().enumerate() {
                if v < 0 {
                    continue;
                }
                let ident = (name.as_str(), s);
                if let Some(prev) = m.old_to_ident.insert((path.as_str(), v), ident) {
                    ensure!(prev == ident, "conflicting old symbol index {path}:{v}");
                }
            }
            old_vals.insert((path.as_str(), name.as_str()), vals.as_slice());
        }
        for (path, name, vals) in d.new_lsyms {
            m.new_vals.insert((path.as_str(), name.as_str()), vals.as_slice());
        }
    }
    for k in old_vals.keys().chain(m.new_vals.keys()) {
        if old_vals.get(k) != m.new_vals.get(k) {
            m.moved_paths.insert(k.0);
        }
    }
    Ok(m)
}

struct Walk<'w, 'x> {
    p: &'x Export<'x>,
    o: &'x Export<'x>,
    n: &'x Export<'x>,
    seen: &'w mut FxHashSet<(Section, u32, u32, u32)>,
    decided: &'w mut FxHashMap<(Section, u32), &'x [u8]>,
    replaced: &'w mut FxHashMap<usize, Vec<u8>>,
}

impl Walk<'_, '_> {
    fn run(&mut self, k0: Section, ip0: u32, io0: u32, in0: u32) -> Result<()> {
        let mut stack = vec![(k0, ip0, io0, in0)];
        while let Some((k, ip, io, inn)) = stack.pop() {
            if !self.seen.insert((k, ip, io, inn)) {
                continue;
            }
            if k == Section::Pkg {
                continue;
            }
            if k == Section::String {
                let (sp, so, sn) = (self.p.pb.string(ip)?, self.o.pb.string(io)?, self.n.pb.string(inn)?);
                ensure!(sp == so, "copied string differs from its source");
                ensure!(so == sn, "string changed in dependency");
                continue;
            }
            let ep = self.p.pb.elem(k, ip)?;
            let eo = self.o.pb.elem(k, io)?;
            let en = self.n.pb.elem(k, inn)?;
            ensure!(
                ep.relocs.len() == eo.relocs.len() && eo.relocs.len() == en.relocs.len(),
                "{k:?} reloc shape differs"
            );
            for ((rp, ro), rn) in ep.relocs.iter().zip(&eo.relocs).zip(&en.relocs) {
                ensure!(rp.kind == ro.kind && ro.kind == rn.kind, "{k:?} reloc kinds differ");
                stack.push((rp.kind, rp.idx, ro.idx, rn.idx));
            }
            if k == Section::Obj {
                for s in [Section::Name, Section::ObjExt, Section::ObjDict] {
                    stack.push((s, ip, io, inn));
                }
            }
            ensure!(ep.payload == eo.payload, "{k:?} copy differs from its source");
            if let Some(prev) = self.decided.get(&(k, ip)) {
                ensure!(*prev == en.payload, "{k:?}:{ip} has two sources that now disagree");
                continue;
            }
            self.decided.insert((k, ip), en.payload);
            if eo.payload != en.payload {
                ensure!(
                    self.o.masked_payload(k, io)? == self.n.masked_payload(k, inn)?,
                    "{k:?} element changed beyond positions and symbol indices"
                );
                let mut raw = Vec::new();
                pkgbits::encode_elem(&ep.relocs, en.payload, &mut raw);
                self.replaced.insert(self.p.pb.abs(k, ip)?, raw);
            }
        }
        Ok(())
    }
}

struct PkgdefPatch {
    bytes: Option<Vec<u8>>,
    old_fp: [u8; 8],
    new_fp: [u8; 8],
    equivalent: bool,
}

// The rewritten export data hashes like the old one if every replaced element masks to the same bytes
// when parsed in its new context. Relocs, strings, names and Meta are never replaced.
fn masks_equal(old: &Export<'_>, new_bits: &[u8], self_path: &str, inliner: Inliner, abs: &[usize]) -> bool {
    let Ok(new) = Export::parse(new_bits, self_path, inliner) else { return false };
    abs.iter().all(|&a| {
        let Some((k, idx)) = old.pb.section_of(a) else { return false };
        matches!(k, Section::Obj | Section::ObjExt | Section::Type | Section::PosBase)
            && matches!((old.masked_payload(k, idx), new.masked_payload(k, idx)), (Ok(x), Ok(y)) if x == y)
    })
}

fn patch_pkgdef(pkgdef: &[u8], self_path: &str, changed: &[ChangedDep<'_>]) -> Result<PkgdefPatch> {
    let loc = export::locate(pkgdef)?;
    let ex = timed("p_self_parse", || Export::parse(&pkgdef[loc.bits_start..loc.bits_end], self_path, loc.inliner))?;
    let old_fp = ex.pb.fingerprint;
    let tw = std::time::Instant::now();
    let mut seen: Vec<FxHashSet<(Section, u32, u32, u32)>> = vec![FxHashSet::default(); changed.len()];
    let mut decided: FxHashMap<(Section, u32), &[u8]> = FxHashMap::default();
    let mut replaced: FxHashMap<usize, Vec<u8>> = FxHashMap::default();
    for (i, id) in ex.idents.iter().enumerate() {
        if id.path == self_path || id.tag == export::Tag::Stub {
            continue;
        }
        let mut src = None;
        for (di, d) in changed.iter().enumerate() {
            match d.old {
                Some(o) => {
                    if let Some(io) = o.find(&id.path, &id.name) {
                        src = Some((di, d, o, io));
                        break;
                    }
                }
                None => ensure!(
                    d.new.find(&id.path, &id.name).is_none(),
                    "{}.{} copied from {} without its old export data",
                    id.path,
                    id.name,
                    d.new.self_path
                ),
            }
        }
        let Some((di, d, old, io)) = src else { continue };
        let inn = d
            .new
            .find(&id.path, &id.name)
            .with_context(|| format!("copied {}.{} vanished from {}", id.path, id.name, old.self_path))?;
        Walk { p: &ex, o: old, n: d.new, seen: &mut seen[di], decided: &mut decided, replaced: &mut replaced }.run(
            Section::Obj,
            u32::try_from(i)?,
            io,
            inn,
        )?;
    }
    crate::store::add_phase("p_walk", tw.elapsed().as_secs_f64() * 1000.0);
    if replaced.is_empty() {
        return Ok(PkgdefPatch { bytes: None, old_fp, new_fp: old_fp, equivalent: true });
    }
    let mut abs: Vec<usize> = replaced.keys().copied().collect();
    abs.sort_unstable();
    let elems: Vec<Vec<u8>> =
        (0..ex.pb.elem_ends.len()).map(|a| replaced.remove(&a).unwrap_or_else(|| ex.pb.raw_abs(a).to_vec())).collect();
    let nb = pkgbits::dump(ex.pb.version, ex.pb.flags, &ex.pb.ends_ends, &elems)?;
    let mut new_fp = [0u8; 8];
    new_fp.copy_from_slice(&nb[nb.len() - 8..]);
    let equivalent = timed("p_equiv", || masks_equal(&ex, &nb, self_path, loc.inliner, &abs));
    let tail = &pkgdef[loc.bits_end..];
    let mut out = Vec::with_capacity(loc.bits_start + nb.len() + tail.len());
    out.extend_from_slice(&pkgdef[..loc.bits_start]);
    out.extend_from_slice(&nb);
    out.extend_from_slice(tail);
    Ok(PkgdefPatch { bytes: Some(out), old_fp, new_fp, equivalent })
}

fn moved_types<'n>(d: &ChangedDep<'_>, want: &HashSet<&'n str>) -> Result<HashSet<&'n str>> {
    let mut types = HashSet::new();
    let old = d.old.context("old export data not loaded")?;
    for (oi, id) in old.idents.iter().enumerate() {
        if id.path != old.self_path || id.tag != export::Tag::Type {
            continue;
        }
        let Some(&name) = want.get(id.name.as_str()) else { continue };
        let Some(ni) = d.new.find(&id.path, &id.name) else { continue };
        let oi = u32::try_from(oi)?;
        if old.pb.raw(Section::Obj, oi)? == d.new.pb.raw(Section::Obj, ni)? {
            continue;
        }
        let os = old.pos_slots(Section::Obj, oi)?;
        let ns = d.new.pos_slots(Section::Obj, ni)?;
        if os.len() != ns.len() || os.iter().zip(&ns).any(|(a, b)| (a.line, a.col) != (b.line, b.col)) {
            types.insert(name);
        }
    }
    Ok(types)
}

fn guard_moved_methods(
    b: &[u8],
    sites: &[usize],
    pkgs: &[String],
    changed: &[ChangedDep<'_>],
    maps: &IndexMaps<'_>,
) -> Result<()> {
    let dep_of: Vec<Option<usize>> = pkgs.iter().map(|p| changed.iter().position(|d| d.new.self_path == *p)).collect();
    let mut calls = Vec::new();
    for &site in sites {
        let r = Obj::symref_at(b, site);
        if !r.is_dep() {
            continue;
        }
        let Some(&Some(di)) = dep_of.get(r.pkg as usize) else { continue };
        if let Some(&(name, slot)) = maps.old_to_ident.get(&(pkgs[r.pkg as usize].as_str(), i64::from(r.idx)))
            && slot >= 2
        {
            calls.push((di, name));
        }
    }
    if calls.is_empty() {
        return Ok(());
    }
    let mut want = vec![HashSet::new(); changed.len()];
    for &(di, name) in &calls {
        want[di].insert(name);
    }
    let mut moved = Vec::with_capacity(changed.len());
    for (d, w) in changed.iter().zip(&want) {
        moved.push(if w.is_empty() { HashSet::new() } else { moved_types(d, w)? });
    }
    match calls.iter().find(|&&(di, name)| moved[di].contains(name)) {
        Some(&(di, name)) => Err(Guard(format!(
            "calls methods of {}.{name}, whose declaration lines moved (wrapper debug info would change)",
            changed[di].new.self_path
        ))
        .into()),
        None => Ok(()),
    }
}

fn remap_symrefs(b: &[u8], sites: &[usize], pkgs: &[String], maps: &IndexMaps<'_>) -> Result<Vec<(usize, SymRef)>> {
    let mut out = Vec::new();
    if maps.moved_paths.is_empty() {
        return Ok(out);
    }
    let moved: Vec<bool> = pkgs.iter().map(|p| maps.moved_paths.contains(p.as_str())).collect();
    for &site in sites {
        let r = Obj::symref_at(b, site);
        if !r.is_dep() {
            continue;
        }
        let pi = r.pkg as usize;
        if !*moved.get(pi).context("symref package out of range")? {
            continue;
        }
        let path = pkgs[pi].as_str();
        let &(name, slot) = maps
            .old_to_ident
            .get(&(path, i64::from(r.idx)))
            .with_context(|| format!("cannot name old symbol {path}:{}", r.idx))?;
        let vals = maps.new_vals.get(&(path, name)).with_context(|| format!("{path}.{name} vanished"))?;
        let v = *vals.get(slot).context("slot vanished")?;
        ensure!(v >= 0, "{path}.{name} is no longer indexed");
        let v = u32::try_from(v).with_context(|| format!("{path}.{name} index {v} out of range"))?;
        if v != r.idx {
            out.push((site, SymRef { pkg: r.pkg, idx: v }));
        }
    }
    Ok(out)
}

struct Rehash {
    affected: Vec<bool>,
    children: Vec<Vec<usize>>,
    old: Vec<[u8; HASH_SIZE]>,
}

fn touches(at: &[usize], lo: usize, hi: usize) -> bool {
    let i = at.partition_point(|&w| w + SYMREF_SIZE <= lo);
    lo < hi && at.get(i).is_some_and(|&w| w < hi)
}

fn affected_hashed(obj: &Obj, b: &[u8], remaps: &[(usize, SymRef)]) -> Option<(Vec<bool>, Vec<Vec<usize>>)> {
    if remaps.is_empty() || obj.n_hashed == 0 {
        return None;
    }
    let at: Vec<usize> = remaps.iter().map(|&(site, _)| site).collect();
    let mut affected: Vec<bool> = (0..obj.n_hashed)
        .map(|j| {
            let g = obj.hashed_global(j);
            let (r0, r1) = obj.reloc_range(b, g);
            let (d0, d1) = obj.data_range(b, g);
            touches(&at, obj.reloc_off(r0), obj.reloc_off(r1)) || touches(&at, d0, d1)
        })
        .collect();
    if !affected.contains(&true) {
        return None;
    }
    let children: Vec<Vec<usize>> = (0..obj.n_hashed).map(|j| obj.hashed_children(b, j)).collect();
    let mut parents = vec![Vec::new(); obj.n_hashed];
    for (j, cs) in children.iter().enumerate() {
        for &c in cs {
            if let Some(p) = parents.get_mut(c) {
                p.push(j);
            }
        }
    }
    let mut stack: Vec<usize> = (0..obj.n_hashed).filter(|&j| affected[j]).collect();
    while let Some(c) = stack.pop() {
        for &p in &parents[c] {
            if !affected[p] {
                affected[p] = true;
                stack.push(p);
            }
        }
    }
    Some((affected, children))
}

fn reproduce_hashes(obj: &Obj, b: &[u8], self_path: &str, remaps: &[(usize, SymRef)]) -> Result<Option<Rehash>> {
    let Some((affected, children)) = affected_hashed(obj, b, remaps) else { return Ok(None) };
    let mut h = Hasher::new(obj, b, self_path, SelfPath::Escaped);
    for j in 0..obj.n_hashed {
        h.over[j] = Some(h.stored(j));
    }
    let mut old = vec![[0u8; HASH_SIZE]; obj.n_hashed];
    for j in (0..obj.n_hashed).filter(|&j| affected[j]) {
        let stored = h.over[j].take();
        old[j] = h.hash(j, 0)?;
        h.over[j] = stored;
    }
    Ok(Some(Rehash { affected, children, old }))
}

fn rehash(obj: &Obj, b: &mut [u8], self_path: &str, plan: &Rehash) -> Result<()> {
    let mut writes = Vec::new();
    {
        let mut h = Hasher::new(obj, b, self_path, SelfPath::Escaped);
        let mut remaining = 0;
        for j in 0..obj.n_hashed {
            if plan.affected[j] {
                remaining += 1;
            } else {
                h.over[j] = Some(h.stored(j));
            }
        }
        while remaining > 0 {
            let mut progressed = false;
            for j in 0..obj.n_hashed {
                if h.over[j].is_some() || plan.children[j].iter().any(|&c| h.over[c].is_none()) {
                    continue;
                }
                let x = h.hash(j, 0)?;
                let stored = h.stored(j);
                let fin = if x == plan.old[j] {
                    stored
                } else {
                    ensure!(plan.old[j] == stored, "hashed symbol {j} changed but its stored hash is not reproducible");
                    writes.push((j, x));
                    x
                };
                h.over[j] = Some(fin);
                remaining -= 1;
                progressed = true;
            }
            ensure!(progressed, "cycle among hashed symbols");
        }
    }
    for (j, x) in writes {
        let o = obj.hash_off(j);
        b[o..o + HASH_SIZE].copy_from_slice(&x);
    }
    Ok(())
}

fn rewrite_build_id(members: &mut [ar::Member], old_buildid: &str, new_buildid: &str) -> Result<()> {
    let old_id = format!("build id \"{old_buildid}\"");
    let new_id = format!("build id \"{new_buildid}\"");
    ensure!(old_id.len() == new_id.len(), "build id length differs");
    for m in members {
        if let Some(at) = export::find_bytes(&m.data, old_id.as_bytes()) {
            m.data[at..at + old_id.len()].copy_from_slice(new_id.as_bytes());
        }
    }
    Ok(())
}

pub(crate) struct Patched {
    pub bytes: Vec<u8>,
    pub remapped_refs: usize,
    /// Old and new export data fingerprints, when the new export data hashes exactly like the old.
    pub equivalent: Option<([u8; 8], [u8; 8])>,
}

pub(crate) struct Target<'a> {
    pub old_archive: &'a [u8],
    pub self_path: &'a str,
    pub old_buildid: &'a str,
    pub new_buildid: &'a str,
    pub full_dwarf: bool,
}

pub(crate) fn patch(t: &Target<'_>, changed: &[ChangedDep<'_>], new_fps: &HashMap<String, [u8; 8]>) -> Result<Patched> {
    let Target { old_archive, self_path, old_buildid, new_buildid, full_dwarf } = *t;
    let mut members = timed("p_ar", || ar::parse(old_archive))?;
    let maps = timed("p_index_maps", || index_maps(changed))?;

    let pi = members.iter().position(|m| m.name == "__.PKGDEF").context("no __.PKGDEF")?;
    let oi = members.iter().position(|m| m.name == "_go_.o").context("no _go_.o")?;

    let pkgdef = timed("p_pkgdef", || patch_pkgdef(&members[pi].data, self_path, changed))?;
    let equivalent = (pkgdef.equivalent && pkgdef.old_fp != pkgdef.new_fp).then_some((pkgdef.old_fp, pkgdef.new_fp));
    if let Some(bytes) = pkgdef.bytes {
        members[pi].data = bytes;
    }

    let mut b = std::mem::take(&mut members[oi].data);
    let (obj, pkgs, sites) = timed("p_sites", || -> Result<_> {
        let obj = Obj::parse(&b)?;
        let pkgs = obj.pkg_index(&b);
        let mut sites = obj.symref_sites(&b);
        sites.sort_unstable();
        sites.dedup();
        Ok((obj, pkgs, sites))
    })?;

    if full_dwarf {
        guard_moved_methods(&b, &sites, &pkgs, changed, &maps)?;
    }
    let fo = obj.fingerprint_off();
    ensure!(b[fo..fo + 8] == pkgdef.old_fp, "object fingerprint does not match export data");
    let remaps = timed("p_remap", || remap_symrefs(&b, &sites, &pkgs, &maps))?;
    let plan = timed("p_rehash", || reproduce_hashes(&obj, &b, self_path, &remaps))?;

    b[fo..fo + 8].copy_from_slice(&pkgdef.new_fp);
    for (pkg, off) in obj.autolib(&b) {
        if let Some(fp) = new_fps.get(&pkg) {
            b[off..off + 8].copy_from_slice(fp);
        }
    }
    for &(site, r) in &remaps {
        Obj::set_symref(&mut b, site, r);
    }
    if let Some(plan) = &plan {
        timed("p_rehash", || rehash(&obj, &mut b, self_path, plan))?;
    }
    members[oi].data = b;

    rewrite_build_id(&mut members, old_buildid, new_buildid)?;
    Ok(Patched { bytes: timed("p_ar", || ar::write(&members)), remapped_refs: remaps.len(), equivalent })
}
