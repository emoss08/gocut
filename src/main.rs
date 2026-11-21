mod ar;
mod export;
mod fx;
mod goobj;
#[cfg(target_os = "linux")]
mod ignore;
mod patch;
mod pkgbits;
mod relink;
mod store;
mod usage;
#[cfg(target_os = "linux")]
mod watch;

use std::cell::OnceCell;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::export::{Export, Inliner};
use crate::store::{Event, LsymTable, Record, Store, env_or, timed};
use crate::usage::Roots;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Shadow,
    Active,
}

fn mode() -> Mode {
    match std::env::var("GOCUT_MODE").as_deref() {
        Ok("off") => Mode::Off,
        Ok("active") => Mode::Active,
        _ => Mode::Shadow,
    }
}

#[derive(Clone, Copy)]
enum Unsupported {
    AsmOrLinkobj,
    Pgo,
    Coverage,
    Cgo,
}

impl Unsupported {
    fn reason(self) -> &'static str {
        match self {
            Unsupported::AsmOrLinkobj => "asm or linkobj",
            Unsupported::Pgo => "pgo",
            Unsupported::Coverage => "coverage",
            Unsupported::Cgo => "generated sources (cgo)",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Unsupported::AsmOrLinkobj => "unsupported-asm",
            Unsupported::Pgo => "unsupported-pgo",
            Unsupported::Coverage => "unsupported-coverage",
            Unsupported::Cgo => "unsupported-cgo",
        }
    }
}

struct CompileArgs {
    out: PathBuf,
    pkg: String,
    importcfg: Option<PathBuf>,
    buildid: String,
    files: Vec<PathBuf>,
    unsupported: Option<Unsupported>,
    norm: Vec<String>,
    embedcfg: Option<PathBuf>,
    full_dwarf: bool,
}

const VALUE_FLAGS: [&str; 5] = ["-o", "-p", "-importcfg", "-buildid", "-embedcfg"];

#[expect(clippy::case_sensitive_file_extension_comparisons, reason = "the go command matches .go case-sensitively")]
fn parse_args(args: &[String]) -> Result<CompileArgs> {
    let mut vals: [Option<&str>; VALUE_FLAGS.len()] = [None; VALUE_FLAGS.len()];
    let mut files = Vec::new();
    let mut unsupported = None;
    let mut full_dwarf = true;
    let mut kept: Vec<(&str, bool)> = Vec::new();
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        if let Some(k) = VALUE_FLAGS.iter().position(|f| *f == a) {
            let v = it.next().context("flag missing value")?;
            vals[k] = Some(v);
            if a == "-p" {
                kept.extend([(a, true), (v, true)]);
            } else {
                kept.push((a, false));
            }
            continue;
        }
        match a {
            "-asmhdr" | "-symabis" | "-linkobj" => unsupported = Some(Unsupported::AsmOrLinkobj),
            _ if a == "-dwarf" || a.starts_with("-dwarf=") => {
                full_dwarf = !matches!(a.strip_prefix("-dwarf="), Some("0" | "f" | "F" | "false" | "FALSE" | "False"));
            }
            _ if a.starts_with("-pgoprofile") => unsupported = Some(Unsupported::Pgo),
            _ if a.starts_with("-coveragecfg") => unsupported = Some(Unsupported::Coverage),
            _ if !a.starts_with('-') && a.ends_with(".go") => files.push(PathBuf::from(a)),
            _ => {}
        }
        if !a.starts_with("-c=") {
            kept.push((a, true));
        }
    }
    let [out, pkg, importcfg, buildid, embedcfg] = vals;
    let out = PathBuf::from(out.context("compile without -o")?);
    let outdir = out.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let mut norm = Vec::with_capacity(kept.len());
    for (a, maskable) in kept {
        if maskable && !outdir.is_empty() && a.contains(outdir.as_str()) {
            if a.ends_with(".go") {
                unsupported = Some(Unsupported::Cgo);
            }
            norm.push(a.replace(outdir.as_str(), "$OUT"));
        } else {
            norm.push(a.to_string());
        }
    }
    Ok(CompileArgs {
        out,
        pkg: pkg.context("compile without -p")?.to_string(),
        importcfg: importcfg.map(PathBuf::from),
        buildid: buildid.context("compile without -buildid")?.to_string(),
        files,
        unsupported,
        norm,
        embedcfg: embedcfg.map(PathBuf::from),
        full_dwarf,
    })
}

#[derive(Default)]
struct ImportCfg {
    files: BTreeMap<String, PathBuf>,
    importmap: BTreeMap<String, String>,
}

fn read_importcfg(p: &Path) -> Result<ImportCfg> {
    let s = std::fs::read_to_string(p)?;
    let mut cfg = ImportCfg::default();
    for line in s.lines() {
        if let Some((k, v)) = line.strip_prefix("packagefile ").and_then(|r| r.split_once('=')) {
            cfg.files.insert(k.to_string(), PathBuf::from(v));
        } else if let Some((k, v)) = line.strip_prefix("importmap ").and_then(|r| r.split_once('=')) {
            cfg.importmap.insert(k.to_string(), v.to_string());
        }
    }
    Ok(cfg)
}

// The GO* variables the compiler may read, sorted so the key does not depend on the order the
// environment lists them in. Compared as bytes: a value need not be UTF-8.
fn go_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut env: Vec<_> = std::env::vars_os()
        .filter(|(k, _)| {
            let k = k.as_encoded_bytes();
            k.starts_with(b"GO") && !k.starts_with(b"GOCUT_") && k != b"GOFLAGS"
        })
        .collect();
    env.sort();
    env
}

fn compile_key(st: &Store, tool: &str, ca: &CompileArgs, cfg: &ImportCfg) -> Result<String> {
    let mut buf = b"gocut-key-2\0".to_vec();
    let md = std::fs::metadata(tool)?;
    buf.extend_from_slice(tool.as_bytes());
    buf.extend_from_slice(&md.len().to_le_bytes());
    if let Some(d) = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
        buf.extend_from_slice(&d.as_nanos().to_le_bytes());
    }
    for (k, v) in go_env() {
        buf.extend_from_slice(k.as_encoded_bytes());
        buf.push(b'=');
        buf.extend_from_slice(v.as_encoded_bytes());
        buf.push(0);
    }
    // Which package each import path names decides what the sources mean, and the usage roots
    // saved with a record are computed through this map.
    for (from, to) in &cfg.importmap {
        buf.extend_from_slice(from.as_bytes());
        buf.push(b'=');
        buf.extend_from_slice(to.as_bytes());
        buf.push(0);
    }
    for a in &ca.norm {
        buf.extend_from_slice(a.as_bytes());
        buf.push(0);
    }
    for f in &ca.files {
        buf.extend_from_slice(f.to_string_lossy().as_bytes());
        buf.extend_from_slice(blake3::hash(&std::fs::read(f)?).as_bytes());
    }
    if let Some(e) = &ca.embedcfg {
        let s = std::fs::read(e)?;
        buf.extend_from_slice(&s);
        let v: serde_json::Value = serde_json::from_slice(&s)?;
        if let Some(files) = v.get("Files").and_then(|f| f.as_object()) {
            for (k, p) in files {
                buf.extend_from_slice(k.as_bytes());
                if let Some(p) = p.as_str() {
                    buf.extend_from_slice(blake3::hash(&std::fs::read(p)?).as_bytes());
                }
            }
        }
    }
    if std::env::var_os("GOCUT_DEBUG_KEY").is_some() {
        let d = st.dir.join("keys");
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(d.join(format!("{}-{}.txt", ca.pkg.replace('/', "_"), std::process::id())), &buf);
    }
    Ok(blake3::hash(&buf).to_hex().to_string())
}

struct DepExport {
    pkgdef: Vec<u8>,
    file: export::ExportFile,
}

struct DepInfo {
    path: PathBuf,
    fp: [u8; 8],
    export: OnceCell<std::result::Result<DepExport, String>>,
}

impl DepInfo {
    fn load(p: &Path) -> Result<DepInfo> {
        Ok(DepInfo { path: p.to_path_buf(), fp: ar::read_pkgdef_fingerprint(p)?, export: OnceCell::new() })
    }

    fn export(&self) -> Result<&DepExport> {
        self.export
            .get_or_init(|| {
                let pkgdef = ar::read_pkgdef(&self.path).map_err(|e| format!("{e:#}"))?;
                let file = export::locate(&pkgdef).map_err(|e| format!("{e:#}"))?;
                Ok(DepExport { pkgdef, file })
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn bits(&self) -> Result<&[u8]> {
        let x = self.export()?;
        Ok(&x.pkgdef[x.file.bits_start..x.file.bits_end])
    }

    fn inliner(&self) -> Result<Inliner> {
        Ok(self.export()?.file.inliner)
    }
}

fn hex8(b: [u8; 8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    b.iter().flat_map(|&x| [HEX[usize::from(x >> 4)], HEX[usize::from(x & 15)]]).map(char::from).collect()
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

struct Prep {
    key: String,
    cfg: ImportCfg,
    deps: BTreeMap<String, DepInfo>,
    fps: BTreeMap<String, String>,
}

fn prepare(st: &Store, tool: &str, ca: &CompileArgs) -> Result<Prep> {
    let cfg = match &ca.importcfg {
        Some(p) => read_importcfg(p)?,
        None => ImportCfg::default(),
    };
    let key = timed("key", || compile_key(st, tool, ca, &cfg))?;
    let mut deps = BTreeMap::new();
    timed("dep_fps", || -> Result<()> {
        for (k, p) in &cfg.files {
            deps.insert(k.clone(), DepInfo::load(p)?);
        }
        Ok(())
    })?;
    let fps = deps.iter().map(|(k, d)| (k.clone(), hex8(d.fp))).collect();
    Ok(Prep { key, cfg, deps, fps })
}

fn compute_roots(ca: &CompileArgs, prep: &Prep) -> Result<Roots> {
    let mut uses = Vec::new();
    for f in &ca.files {
        let mut fu = usage::scan(&std::fs::read(f)?);
        for imp in &mut fu.imports {
            if let Some(m) = prep.cfg.importmap.get(&imp.path) {
                imp.path = m.clone();
            }
        }
        uses.push(fu);
    }
    let name_of = |path: &str| export::pkg_name(prep.deps.get(path)?.bits().ok()?).ok();
    Ok(usage::roots(&uses, &name_of))
}

#[derive(Clone, Copy)]
enum BlockKind {
    ImportSet,
    ExportData,
    DotImport,
    PkgIdentity,
    UsesChanged,
    Guard,
    Evicted,
    PatchFailed,
    PatchPanicked,
}

impl BlockKind {
    fn name(self) -> &'static str {
        match self {
            BlockKind::ImportSet => "import-set",
            BlockKind::ExportData => "export-data",
            BlockKind::DotImport => "dot-import",
            BlockKind::PkgIdentity => "pkg-identity",
            BlockKind::UsesChanged => "uses-changed",
            BlockKind::Guard => "guard",
            BlockKind::Evicted => "evicted",
            BlockKind::PatchFailed => "patch-failed",
            BlockKind::PatchPanicked => "patch-panicked",
        }
    }
}

enum Decision {
    NoRecord,
    Unsupported(Unsupported),
    PrepareFailed(String),
    Blocked { kind: BlockKind, reason: String },
    Reuse { patched: Vec<u8>, remapped: usize, changed: usize, equivalent: Option<patch::Equivalent>, relinked: bool },
}

impl Decision {
    fn blocked(kind: BlockKind, reason: impl Into<String>) -> Decision {
        Decision::Blocked { kind, reason: reason.into() }
    }

    fn name(&self) -> &'static str {
        match self {
            Decision::NoRecord => "no-record",
            Decision::Unsupported(_) | Decision::PrepareFailed(_) => "unsupported",
            Decision::Blocked { .. } => "blocked",
            Decision::Reuse { .. } => "reuse",
        }
    }
}

fn changed_deps(rec: &Record, fps: &BTreeMap<String, String>) -> Option<Vec<String>> {
    if !rec.deps.keys().eq(fps.keys()) {
        return None;
    }
    Some(fps.iter().filter(|&(k, v)| &rec.deps[k] != v).map(|(k, _)| k.clone()).collect())
}

fn pkg_facts(st: &Store, fp: &str, ex: &Export<'_>) -> Result<Facts> {
    if let Some(f) = st.read_pkg_facts(fp) {
        return Ok(f);
    }
    let f = timed("pkg_hash", || -> Result<_> {
        Ok((ex.self_name.clone(), ex.has_init, blake3::Hash::from(ex.pkg_hash()?).to_hex().to_string()))
    })?;
    let _ = st.write_pkg_facts(fp, &f);
    Ok(f)
}

struct HashCache {
    fp: String,
    cur: HashMap<String, String>,
    add: Vec<(String, String)>,
}

impl HashCache {
    fn open(st: &Store, fp: String) -> HashCache {
        HashCache { cur: st.read_hashes(&fp), fp, add: Vec::new() }
    }

    fn get(&mut self, ex: &Export<'_>, path: &str, name: &str) -> Result<String> {
        if let Some(h) = self.cur.get(name) {
            return Ok(h.clone());
        }
        let h = match ex.find(path, name) {
            Some(i) => blake3::Hash::from(ex.obj_hash(i)?).to_hex().to_string(),
            None => "-".to_string(),
        };
        self.cur.insert(name.to_string(), h.clone());
        self.add.push((name.to_string(), h.clone()));
        Ok(h)
    }

    fn flush(self, st: &Store) {
        let _ = st.append_hashes(&self.fp, &self.add);
    }
}

fn lsyms_for(st: &Store, fp: &str, ex: Option<&Export<'_>>) -> Result<LsymTable> {
    if let Some(t) = st.read_lsyms(fp) {
        return Ok(t);
    }
    let t = patch::lsym_table(ex.context("symbol table evicted")?);
    let _ = st.write_lsyms(fp, &t);
    Ok(t)
}

// The non-local names an archive's own export data carries copies of.
fn copied_idents(archive: &[u8], self_path: &str) -> Result<Vec<(String, String)>> {
    let pkgdef = ar::member_data(archive, "__.PKGDEF").context("no __.PKGDEF")?;
    let loc = export::locate(pkgdef)?;
    let ex = Export::parse(&pkgdef[loc.bits_start..loc.bits_end], self_path, loc.inliner)?;
    Ok(ex
        .idents
        .into_iter()
        .filter(|id| id.path != self_path && id.tag != export::Tag::Stub)
        .map(|id| (id.path, id.name))
        .collect())
}

// A dependency whose export data changed since the record, as decide sees it.
struct Dep<'x> {
    p: &'x str,
    ofp: &'x str,
    nfp: &'x str,
    new: Export<'x>,
    // Old and new fingerprints after following .same: equal when only positions and symbol indices moved.
    canon: (String, String),
    // The fingerprints the old and new name hashes are kept under: equal when every name hashes
    // the same, whether or not the import graph changed.
    like: (String, String),
    // The old export data's package facts, when the store has them.
    old_facts: Option<Facts>,
    // gocut rebuilt the new export data from the old (in one step or several) with nothing but the
    // import graph changed.
    same_elements: bool,
    need_old: bool,
}

// A package's name, whether it has an init task, and the hash of its import graph.
type Facts = (String, bool, String);

impl Dep<'_> {
    fn names_same(&self) -> bool {
        self.like.0 == self.like.1
    }
}

// A changed dependency that passed decide's checks, as the patch takes it.
struct Pair<'x> {
    ofp: &'x str,
    nfp: &'x str,
    old: Option<Export<'x>>,
    new: &'x Export<'x>,
    imports_changed: bool,
    // The dependency's objects and symbol indices are as they were; only its import graph changed.
    objects_unchanged: bool,
}

// A changed dependency gocut cannot reason about at all: export data it does not read, or a dot
// import, whose uses the source scan cannot see.
fn refuse_changed(rec: &Record, prep: &Prep, changed: &[String]) -> Option<Decision> {
    for p in changed {
        if let Err(e) = prep.deps[p].bits().and_then(pkgbits::PkgBits::parse) {
            return Some(Decision::blocked(BlockKind::ExportData, format!("export data of {p}: {e:#}")));
        }
    }
    changed
        .iter()
        .find(|p| rec.roots.all.contains(*p))
        .map(|p| Decision::blocked(BlockKind::DotImport, format!("dot import of {p}")))
}

// Parses each changed dependency's new export data and reads the store's links for it.
fn load_deps<'x>(st: &Store, rec: &'x Record, prep: &'x Prep, changed: &'x [String]) -> Result<Vec<Dep<'x>>> {
    let mut ds = Vec::with_capacity(changed.len());
    for p in changed {
        let (d, ofp, nfp) = (&prep.deps[p], rec.deps[p].as_str(), prep.fps[p].as_str());
        let ni = timed("read_new_export", || d.inliner())?;
        let new = timed("parse_dep", || Export::parse(d.bits()?, p, ni))?;
        let _ = timed("write_export", || st.write_export(nfp, d.bits()?));
        let canon = (st.canonical(ofp), st.canonical(nfp));
        let (like, old_facts, same_elements) = if canon.0 == canon.1 {
            (canon.clone(), None, false)
        } else {
            let like = (st.objects_like(&canon.0), st.objects_like(&canon.1));
            (like, st.read_pkg_facts(&canon.0), st.elems_root(ofp) == st.elems_root(nfp))
        };
        ds.push(Dep { p, ofp, nfp, new, canon, like, old_facts, same_elements, need_old: true });
    }
    Ok(ds)
}

// Checks one changed dependency: Ok(Err) is a decision not to reuse.
fn check_dep<'x>(
    st: &Store,
    rec: &Record,
    d: &'x Dep<'x>,
    old: Option<Export<'x>>,
) -> Result<std::result::Result<Pair<'x>, Decision>> {
    let p = d.p;
    let (oc, nc) = &d.canon;
    if oc == nc {
        // gocut produced the new export data from the old by moving positions only; it hashes the same.
        let pair = Pair { ofp: d.ofp, nfp: d.nfp, old, new: &d.new, imports_changed: false, objects_unchanged: false };
        return Ok(Ok(pair));
    }
    let of = match (&d.old_facts, &old) {
        (Some(f), _) => f.clone(),
        (None, Some(old)) => pkg_facts(st, oc, old)?,
        (None, None) => anyhow::bail!("old export data of {p} not loaded"),
    };
    let nf = pkg_facts(st, nc, &d.new)?;
    if of.0 != nf.0 || of.1 != nf.1 {
        return Ok(Err(Decision::blocked(BlockKind::PkgIdentity, format!("{p}: package name or init changed"))));
    }
    // A changed import graph does not change what this package compiles to, only the copy of
    // the graph in its export data, which the patch rebuilds.
    let imports_changed = of.2 != nf.2;
    let Some(names) = rec.roots.names.get(p) else {
        // Every import the sources name has an entry, so this dependency is one the go command
        // added. Nothing says what the package takes from it.
        let reason = format!("{p} changed and is not imported by name");
        return Ok(Err(Decision::blocked(BlockKind::UsesChanged, reason)));
    };
    // Export data gocut rebuilt around a changed import graph hashes like the one it came from.
    if !d.names_same() {
        let old = old.as_ref().context("old export data not loaded")?;
        let t = Instant::now();
        let (mut oh, mut nh) = (HashCache::open(st, d.like.0.clone()), HashCache::open(st, d.like.1.clone()));
        let mut differs = None;
        for n in names {
            if oh.get(old, p, n)? != nh.get(&d.new, p, n)? {
                differs = Some(n);
                break;
            }
        }
        oh.flush(st);
        nh.flush(st);
        store::add_phase("uses", ms(t));
        if let Some(n) = differs {
            return Ok(Err(Decision::blocked(BlockKind::UsesChanged, format!("uses changed {p}.{n}"))));
        }
    }
    let objects_unchanged = d.same_elements && old.is_none();
    Ok(Ok(Pair { ofp: d.ofp, nfp: d.nfp, old, new: &d.new, imports_changed, objects_unchanged }))
}

fn decide(
    st: &Store,
    ca: &CompileArgs,
    rec: &Record,
    prep: &Prep,
    changed: &[String],
    olds: &mut HashMap<String, Vec<u8>>,
) -> Result<Decision> {
    if let Some(d) = refuse_changed(rec, prep, changed) {
        return Ok(d);
    }
    let mut ds = load_deps(st, rec, prep, changed)?;
    let mut old_out = None;
    // A dependency whose names hash like its old ones needs the old export data only to work out
    // package facts the store does not have and, unless its objects are the old ones outright, to
    // walk objects this package copies from it or to rebuild a missing symbol table.
    if !ca.full_dwarf && ds.iter().any(Dep::names_same) {
        let copied = if ds.iter().any(|d| d.names_same() && !d.same_elements) {
            let out = old_out.insert(timed("read_blob", || st.read_blob(&rec.out))?);
            timed("copied", || copied_idents(out, &ca.pkg))?
        } else {
            Vec::new()
        };
        for d in ds.iter_mut().filter(|d| d.names_same()) {
            let facts_missing = d.canon.0 != d.canon.1 && d.old_facts.is_none();
            let objects_needed = !d.same_elements
                && (!st.has_lsyms(d.ofp) || copied.iter().any(|(path, name)| d.new.find(path, name).is_some()));
            d.need_old = facts_missing || objects_needed;
        }
    }
    for d in ds.iter().filter(|d| d.need_old) {
        if let Entry::Vacant(slot) = olds.entry(d.ofp.to_string()) {
            slot.insert(
                timed("read_old_export", || st.read_export(d.ofp))
                    .with_context(|| format!("old export data of {}", d.p))?,
            );
        }
    }
    let olds = &*olds;
    let mut pairs = Vec::with_capacity(ds.len());
    for d in &ds {
        let old = if d.need_old {
            Some(timed("parse_dep", || Export::parse(&olds[d.ofp], d.p, d.new.inliner()))?)
        } else {
            None
        };
        match check_dep(st, rec, d, old)? {
            Ok(pair) => pairs.push(pair),
            Err(decision) => return Ok(decision),
        }
    }
    let lsyms: Vec<Option<(LsymTable, LsymTable)>> = timed("lsyms", || {
        pairs
            .iter()
            .map(|p| {
                if p.objects_unchanged {
                    return Ok(None);
                }
                Ok(Some((lsyms_for(st, p.ofp, p.old.as_ref())?, lsyms_for(st, p.nfp, Some(p.new))?)))
            })
            .collect::<Result<_>>()
    })?;
    let deps: Vec<patch::ChangedDep<'_>> = pairs
        .iter()
        .zip(&lsyms)
        .map(|(p, l)| patch::ChangedDep {
            old: p.old.as_ref(),
            new: p.new,
            lsyms: l.as_ref().map(|(ol, nl)| (ol, nl)),
            imports_changed: p.imports_changed,
        })
        .collect();
    let new_fps: HashMap<String, [u8; 8]> = prep.deps.iter().map(|(k, d)| (k.clone(), d.fp)).collect();
    let old_out = match old_out {
        Some(o) => o,
        None => timed("read_blob", || st.read_blob(&rec.out))?,
    };
    let target = patch::Target {
        old_archive: &old_out,
        self_path: &ca.pkg,
        old_buildid: &rec.buildid,
        new_buildid: &ca.buildid,
        full_dwarf: ca.full_dwarf,
    };
    let p = timed("patch", || patch::patch(&target, &deps, &new_fps))?;
    Ok(Decision::Reuse {
        patched: p.bytes,
        remapped: p.remapped_refs,
        changed: deps.len(),
        equivalent: p.equivalent,
        relinked: p.relinked,
    })
}

fn is_evicted(e: &anyhow::Error) -> bool {
    e.root_cause().downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

fn try_decide(
    st: &Store,
    ca: &CompileArgs,
    rec: &Record,
    prep: &Prep,
    changed: &[String],
    olds: &mut HashMap<String, Vec<u8>>,
) -> Decision {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decide(st, ca, rec, prep, changed, olds))) {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => {
            if let Some(g) = e.downcast_ref::<patch::Guard>() {
                Decision::blocked(BlockKind::Guard, g.0.clone())
            } else {
                let kind = if is_evicted(&e) { BlockKind::Evicted } else { BlockKind::PatchFailed };
                Decision::blocked(kind, format!("patch failed: {e:#}"))
            }
        }
        Err(_) => Decision::blocked(BlockKind::PatchPanicked, "patch panicked"),
    }
}

// The records' indices with each one's changed dependencies (None when its import set differs),
// fewest changes first and the more recent record first among equals.
fn closest_first(recs: &[Record], fps: &BTreeMap<String, String>) -> Vec<(usize, Option<Vec<String>>)> {
    let mut order: Vec<(usize, Option<Vec<String>>)> =
        recs.iter().enumerate().map(|(i, rec)| (i, changed_deps(rec, fps))).collect();
    order.sort_by_key(|(i, changed)| (changed.as_ref().map_or(usize::MAX, Vec::len), *i));
    order
}

// Tries the saved records, the one with the fewest changed dependencies first: it is the cheapest
// to patch and the least likely to be blocked. The most recent record is often not that one: after
// an edit is undone, which the go command serves from its cache without compiling, the newest
// record is still the edited state. Returns the decision, how many records were tried, and which
// record allowed reuse. When none did, the reason is the one from the closest record.
fn choose(st: &Store, ca: &CompileArgs, recs: &[Record], prep: &Prep) -> (Decision, usize, Option<usize>) {
    let n = recs.len().min(store::max_records());
    let mut olds = HashMap::new();
    let mut closest: Option<(BlockKind, String)> = None;
    for (tried, (i, changed)) in closest_first(&recs[..n], &prep.fps).into_iter().enumerate() {
        let d = match changed {
            None => Decision::blocked(BlockKind::ImportSet, "import set changed"),
            Some(c) => try_decide(st, ca, &recs[i], prep, &c, &mut olds),
        };
        match d {
            Decision::Blocked { kind, reason } => {
                closest.get_or_insert((kind, reason));
            }
            d => return (d, tried + 1, Some(i)),
        }
    }
    match closest {
        None => (Decision::NoRecord, 0, None),
        Some((kind, r)) => {
            (Decision::blocked(kind, format!("{r} (tried {n} record{})", if n == 1 { "" } else { "s" })), n, None)
        }
    }
}

fn sampled(ca: &CompileArgs) -> bool {
    let pct = env_or("GOCUT_VERIFY", 10u32).min(100);
    let h = blake3::hash(format!("{}\0{}", ca.pkg, ca.buildid).as_bytes());
    (u32::from(h.as_bytes()[0]) * 100) / 256 < pct
}

fn exec_tool(tool: &str, args: &[String]) -> anyhow::Error {
    let err = Command::new(tool).args(args).exec();
    anyhow::anyhow!("exec {tool}: {err}")
}

// A dependency's export data the store does not hold yet.
struct NewExport {
    path: String,
    fp: String,
    bits: Vec<u8>,
    inliner: Inliner,
}

struct Pending {
    key: String,
    rec: Record,
    recs: Vec<Record>,
    exports: Vec<NewExport>,
}

// Reads every input the record needs now, while the sources and dependency archives are as compiled.
// `reused` is the record that allowed reuse, if one did. Its roots still hold: the sources and the
// import map are the same (they are in the key), and every dependency that changed since kept its
// package name. After any other decision the names may have changed, so the sources are scanned again.
fn pending_record(
    st: &Store,
    ca: &CompileArgs,
    prep: &Prep,
    recs: Vec<Record>,
    reused: Option<usize>,
) -> Result<Pending> {
    let roots = match reused.and_then(|i| recs.get(i)) {
        Some(r) => r.roots.clone(),
        None => compute_roots(ca, prep)?,
    };
    let mut exports = Vec::new();
    for (k, d) in &prep.deps {
        let fp = &prep.fps[k];
        if !st.has_export(fp) {
            exports.push(NewExport {
                path: k.clone(),
                fp: fp.clone(),
                bits: d.bits()?.to_vec(),
                inliner: d.inliner()?,
            });
        }
    }
    let rec =
        Record { pkg: ca.pkg.clone(), buildid: ca.buildid.clone(), out: String::new(), deps: prep.fps.clone(), roots };
    Ok(Pending { key: prep.key.clone(), rec, recs, exports })
}

fn save_record(st: &Store, mut p: Pending, out: &[u8]) -> Result<()> {
    timed("rec_exports", || p.exports.iter().try_for_each(|e| st.write_export(&e.fp, &e.bits)))?;
    p.rec.out = timed("rec_blob", || st.write_blob(out))?;
    timed("rec_json", || st.write_record(&p.key, &p.rec, &p.recs))?;
    // With the record saved, work out what the next build would otherwise work out while the go
    // command waits: each new dependency's package facts and symbol table. Export data gocut
    // does not read has neither.
    for e in &p.exports {
        if let Ok(ex) = Export::parse(&e.bits, &e.path, e.inliner) {
            let _ = pkg_facts(st, &st.canonical(&e.fp), &ex);
            let _ = lsyms_for(st, &e.fp, Some(&ex));
        }
    }
    Ok(())
}

// Saving the record only matters to later builds, so it runs in a detached child and the compile
// step returns as soon as its output is in place. Returns true in the child, false in the parent
// (which should exit now), and true without forking if detaching is off or fails.
fn detach() -> bool {
    if std::env::var("GOCUT_DETACH").is_ok_and(|v| v == "0") {
        return true;
    }
    // SAFETY: gocut is single-threaded here; the child only runs ordinary Rust code afterwards.
    match unsafe { libc::fork() } {
        0 => {
            // SAFETY: setsid takes no arguments; the child is not a group leader, so it succeeds.
            unsafe { libc::setsid() };
            // SAFETY: the path is a NUL-terminated literal.
            let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
            if null >= 0 {
                for fd in 0..3 {
                    // SAFETY: null is an open descriptor this process owns; fd is 0, 1 or 2.
                    unsafe { libc::dup2(null, fd) };
                }
                if null > 2 {
                    // SAFETY: null is open, owned here, and no longer used.
                    unsafe { libc::close(null) };
                }
            }
            true
        }
        pid if pid > 0 => false,
        _ => true,
    }
}

#[expect(clippy::too_many_lines, reason = "one linear pipeline from compile arguments to the logged event")]
fn toolexec(tool: &str, args: &[String]) -> Result<ExitCode> {
    let m = mode();
    let base = Path::new(tool).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if m == Mode::Off || base != "compile" || args.iter().any(|a| a.starts_with("-V")) {
        return Err(exec_tool(tool, args));
    }
    let t0 = Instant::now();
    let (Ok(ca), Ok(st)) = (parse_args(args), Store::open()) else {
        return Err(exec_tool(tool, args));
    };
    let mut ev = Event::new(&ca.pkg);

    let prep = match ca.unsupported {
        Some(u) => Err(Decision::Unsupported(u)),
        None => prepare(&st, tool, &ca).map_err(|e| Decision::PrepareFailed(format!("prepare failed: {e:#}"))),
    };
    ev.prep_ms = ms(t0);
    let (prep, recs, decision, tried, used) = match prep {
        Ok(p) => {
            let recs = st.read_records(&p.key);
            let (d, tried, used) = choose(&st, &ca, &recs, &p);
            (Some(p), recs, d, tried, used)
        }
        Err(d) => (None, Vec::new(), d, 0, None),
    };
    ev.check_ms = ms(t0);
    ev.decide_ms = ev.check_ms - ev.prep_ms;

    ev.decision = decision.name().to_string();
    ev.tried = tried;
    let mut equivalent = None;
    let mut reused = None;
    let patched = match decision {
        Decision::NoRecord => None,
        Decision::Unsupported(u) => {
            ev.kind = u.kind().into();
            ev.reason = u.reason().into();
            None
        }
        Decision::PrepareFailed(r) => {
            ev.kind = "prepare-failed".into();
            ev.reason = r;
            None
        }
        Decision::Blocked { kind, reason } => {
            ev.kind = kind.name().into();
            ev.reason = reason;
            None
        }
        Decision::Reuse { patched, remapped, changed, equivalent: eq, relinked } => {
            ev.record = used.unwrap_or(0);
            reused = used;
            ev.remapped = remapped;
            ev.changed_deps = changed;
            ev.relinked = relinked;
            equivalent = eq;
            Some(patched)
        }
    };
    let patched_len = patched.as_ref().map(Vec::len);

    // In active mode a reuse outside the verified sample is written straight to the output. If
    // that fails the compiler runs after all: it reports the problem in its own words, or succeeds.
    let mut patched = patched;
    let mut written = None;
    if m == Mode::Active
        && !sampled(&ca)
        && let Some(p) = patched.take()
    {
        match timed("write_out", || std::fs::write(&ca.out, &p)) {
            Ok(()) => written = Some(p),
            Err(e) => {
                ev.decision = "blocked".into();
                ev.kind = "write-failed".into();
                ev.reason = format!("writing the reused archive: {e}");
            }
        }
    }
    let output = if let Some(p) = written {
        p
    } else {
        if m == Mode::Active && patched.is_some() {
            ev.decision = "reuse+verify".into();
        }
        let t1 = Instant::now();
        let status = Command::new(tool).args(args).status()?;
        ev.compile_ms = ms(t1);
        ev.compiled = true;
        if !status.success() {
            ev.decision = format!("{}+compile-failed", ev.decision);
            ev.phases = store::take_phases();
            st.log(&ev);
            // A Unix exit code is 0 to 255; a signal death has none and counts as 1.
            let code = status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1);
            return Ok(ExitCode::from(code));
        }
        // The build has its output; without a copy of it there is only no record to save.
        let real = match std::fs::read(&ca.out) {
            Ok(real) => real,
            Err(e) => {
                ev.reason = format!("{} reading the compiler's output: {e}", ev.reason);
                ev.total_ms = ms(t0);
                ev.phases = store::take_phases();
                st.log(&ev);
                return Ok(ExitCode::SUCCESS);
            }
        };
        if let Some(p) = &patched {
            if *p == real {
                ev.shadow = "match".into();
            } else {
                let n = p.len().max(real.len());
                let diff = (0..n).filter(|&i| p.get(i) != real.get(i)).count();
                ev.shadow = format!("mismatch:{diff}");
                st.save_mismatch(&ca.pkg, p, &real);
                if m == Mode::Active {
                    eprintln!(
                        "gocut: verification mismatch for {} ({diff} bytes); used the real compile. Report this.",
                        ca.pkg
                    );
                }
            }
        }
        real
    };

    // The output is the patched archive (active reuse, or a verified match): its export data hashes like the old.
    if let Some(eq) = equivalent
        && (ev.shadow == "match" || (!ev.compiled && patched_len == Some(output.len())))
    {
        let (old, new) = (st.canonical(&hex8(eq.old)), hex8(eq.new));
        let _ = match eq.link {
            patch::Link::Same => st.link_same(&new, &old),
            patch::Link::Objects => st.link_like(&st.canonical(&new), &st.objects_like(&old)),
        };
        if eq.same_elements {
            let _ = st.link_elems(&new, &st.elems_root(&hex8(eq.old)));
        }
    }
    let pending = prep.as_ref().map(|p| timed("rec_read", || pending_record(&st, &ca, p, recs, reused)));
    ev.total_ms = ms(t0);
    ev.phases = store::take_phases();
    let pending = match pending {
        Some(Err(e)) => {
            ev.reason = format!("{} record failed: {e:#}", ev.reason);
            None
        }
        Some(Ok(p)) => Some(p),
        None => None,
    };
    st.log(&ev);
    if !detach() {
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(p) = pending
        && let Err(e) = save_record(&st, p, &output)
    {
        let mut fail = Event::new(&ca.pkg);
        fail.decision = "record-failed".into();
        fail.reason = format!("record failed: {e:#}");
        st.log(&fail);
    }
    st.maybe_evict();
    Ok(ExitCode::SUCCESS)
}

fn dumphash(archive: &str, pkg: &str) -> Result<()> {
    let b = std::fs::read(archive)?;
    let members = ar::parse(&b)?;
    let o = &ar::member(&members, "_go_.o").context("no _go_.o")?.data;
    let obj = goobj::Obj::parse(o)?;
    println!(
        "defs {} hashed64 {} hashed {} nonpkgdef {} pkgs {:?}",
        obj.n_def,
        obj.n_h64,
        obj.n_hashed,
        obj.n_nonpkgdef,
        obj.pkg_index(o)
    );
    for spelling in [goobj::SelfPath::Escaped, goobj::SelfPath::Raw] {
        let esc = spelling == goobj::SelfPath::Escaped;
        let mut h = goobj::Hasher::new(&obj, o, pkg, spelling);
        let mut bad = 0;
        for j in 0..obj.n_hashed {
            let ok = h.hash(j, 0)? == h.stored(j);
            if !ok {
                bad += 1;
                if bad <= 8 {
                    let g = obj.hashed_global(j);
                    println!("  esc={esc} MISMATCH #{j} type={} name={}", obj.sym_type(o, g), obj.sym_name(o, g));
                }
            }
        }
        println!("esc={esc}: {bad} of {} hashed symbols mismatch", obj.n_hashed);
    }
    Ok(())
}

// Debugging: whether gocut can rebuild an archive's export data layout exactly (see relink.rs).
fn relinkcheck(archive: &str, pkg: &str) -> Result<()> {
    let pkgdef = ar::read_pkgdef(Path::new(archive))?;
    let loc = export::locate(&pkgdef)?;
    let bits = &pkgdef[loc.bits_start..loc.bits_end];
    let t = Instant::now();
    let ex = Export::parse(bits, pkg, loc.inliner)?;
    let parsed = ms(t);
    relink::reproduce(&ex, bits)?;
    println!(
        "{pkg}: layout reproduced ({} bytes of export data; parsed in {parsed:.1} ms, rebuilt in {:.1} ms)",
        bits.len(),
        ms(t) - parsed
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn watch_cmd(args: &[String]) -> Result<ExitCode> {
    watch::run(watch::parse_options(args)?)
}

#[cfg(not(target_os = "linux"))]
fn watch_cmd(_: &[String]) -> Result<ExitCode> {
    anyhow::bail!("gocut watch needs Linux (inotify and signalfd)")
}

// Bytes in GiB, for display.
#[expect(clippy::cast_precision_loss, reason = "display only; two decimals of a GiB count")]
fn gib(n: u64) -> f64 {
    n as f64 / (1u64 << 30) as f64
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("stats") => store::stats(args.get(1).map(String::as_str)).map(|()| ExitCode::SUCCESS),
        Some("gc") if args.get(1).is_some_and(|a| a == "--auto") => Store::open().map(|s| {
            s.auto_gc();
            ExitCode::SUCCESS
        }),
        Some("gc") => Store::open().and_then(|s| s.gc()).map(|(a, b)| {
            println!("store: {:.2} GiB -> {:.2} GiB", gib(a), gib(b));
            ExitCode::SUCCESS
        }),
        Some("clear-log") => Store::open().and_then(|s| s.clear_log()).map(|()| ExitCode::SUCCESS),
        Some("dumphash") if args.len() == 3 => dumphash(&args[1], &args[2]).map(|()| ExitCode::SUCCESS),
        Some("relinkcheck") if args.len() == 3 => relinkcheck(&args[1], &args[2]).map(|()| ExitCode::SUCCESS),
        Some("watch") => watch_cmd(&args[1..]),
        Some("version" | "--version" | "-V") => {
            let versions: Vec<String> = pkgbits::SUPPORTED_VERSIONS.iter().map(u32::to_string).collect();
            println!(
                "gocut {}\nreads export data versions {} ({})",
                env!("CARGO_PKG_VERSION"),
                versions.join(", "),
                pkgbits::SUPPORTED_GO
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(t) if t.contains('/') => toolexec(t, &args[1..]),
        _ => {
            eprintln!(
                "usage: go build -toolexec=gocut ... | gocut stats [since-seconds] | gocut gc | gocut clear-log | gocut watch ... | gocut version"
            );
            Ok(ExitCode::from(2))
        }
    };
    match r {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gocut: {e:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(deps: &[(&str, &str)]) -> Record {
        Record {
            pkg: "ex/p".into(),
            buildid: String::new(),
            out: String::new(),
            deps: deps.iter().map(|&(k, v)| (k.to_string(), v.to_string())).collect(),
            roots: Roots::default(),
        }
    }

    // After an edit is undone the newest record is the edited state; the next edit should start
    // from the record nearest to what is being built, not from the newest.
    #[test]
    fn the_record_with_the_fewest_changed_dependencies_is_tried_first() {
        let now: BTreeMap<String, String> =
            [("ex/a", "a2"), ("ex/b", "b1"), ("ex/c", "c1")].iter().map(|&(k, v)| (k.into(), v.into())).collect();
        let recs = [
            rec(&[("ex/a", "a9"), ("ex/b", "b9"), ("ex/c", "c9")]),
            rec(&[("ex/a", "a1"), ("ex/b", "b1"), ("ex/c", "c1")]),
            rec(&[("ex/a", "a1"), ("ex/b", "b1")]),
            rec(&[("ex/a", "a1"), ("ex/b", "b1"), ("ex/c", "c1")]),
        ];
        let order = closest_first(&recs, &now);
        let indices: Vec<usize> = order.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, [1, 3, 0, 2], "one change (newer first), then three, then another import set");
        assert_eq!(order[0].1.as_deref(), Some(&["ex/a".to_string()][..]), "only ex/a differs from record 1");
        assert_eq!(order[3].1, None, "record 2 imports a different set of packages");
    }

    #[test]
    fn the_key_environment_is_sorted_and_leaves_out_gocut_settings() {
        let env = go_env();
        assert!(env.is_sorted(), "the key must not depend on the order of the environment");
        assert!(
            env.iter().all(|(k, _)| {
                let k = k.as_encoded_bytes();
                k.starts_with(b"GO") && !k.starts_with(b"GOCUT_") && k != b"GOFLAGS"
            }),
            "only GO* variables that are not gocut's own or GOFLAGS"
        );
    }
}
