mod ar;
mod export;
mod goobj;
mod patch;
mod pkgbits;
mod store;
mod usage;

use std::cell::OnceCell;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::export::Export;
use crate::store::{Event, LsymTable, Record, Store, env_or};
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
    importmap: HashMap<String, String>,
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

fn compile_key(st: &Store, tool: &str, ca: &CompileArgs) -> Result<String> {
    let mut buf = b"gocut-key-1\0".to_vec();
    let md = std::fs::metadata(tool)?;
    buf.extend_from_slice(tool.as_bytes());
    buf.extend_from_slice(&md.len().to_le_bytes());
    if let Some(d) = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
        buf.extend_from_slice(&d.as_nanos().to_le_bytes());
    }
    for (k, v) in std::env::vars() {
        if k.starts_with("GO") && !k.starts_with("GOCUT_") && k != "GOFLAGS" {
            buf.extend_from_slice(k.as_bytes());
            buf.push(b'=');
            buf.extend_from_slice(v.as_bytes());
            buf.push(0);
        }
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

    fn new_inliner(&self) -> Result<bool> {
        Ok(self.export()?.file.new_inliner)
    }
}

fn hex8(b: &[u8; 8]) -> String {
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
    let key = compile_key(st, tool, ca)?;
    let cfg = match &ca.importcfg {
        Some(p) => read_importcfg(p)?,
        None => ImportCfg::default(),
    };
    let mut deps = BTreeMap::new();
    for (k, p) in &cfg.files {
        deps.insert(k.clone(), DepInfo::load(p)?);
    }
    let fps = deps.iter().map(|(k, d)| (k.clone(), hex8(&d.fp))).collect();
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
    DotImport,
    PkgIdentity,
    ImportGraph,
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
            BlockKind::DotImport => "dot-import",
            BlockKind::PkgIdentity => "pkg-identity",
            BlockKind::ImportGraph => "import-graph",
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
    Reuse { patched: Vec<u8>, remapped: usize, changed: usize },
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

struct HashCache<'a> {
    fp: &'a str,
    cur: HashMap<String, String>,
    add: Vec<(String, String)>,
}

impl<'a> HashCache<'a> {
    fn open(st: &Store, fp: &'a str) -> HashCache<'a> {
        HashCache { fp, cur: st.read_hashes(fp), add: Vec::new() }
    }

    fn get(&mut self, ex: &Export, path: &str, name: &str) -> Result<String> {
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
        let _ = st.append_hashes(self.fp, &self.add);
    }
}

fn lsyms_for(st: &Store, fp: &str, ex: &Export) -> LsymTable {
    st.read_lsyms(fp).unwrap_or_else(|| {
        let t = patch::lsym_table(ex);
        let _ = st.write_lsyms(fp, &t);
        t
    })
}

fn decide(
    st: &Store,
    ca: &CompileArgs,
    rec: &Record,
    prep: &Prep,
    changed: &[String],
    olds: &mut HashMap<String, Vec<u8>>,
) -> Result<Decision> {
    for p in changed {
        if rec.roots.all.contains(p) {
            return Ok(Decision::blocked(BlockKind::DotImport, format!("dot import of {p}")));
        }
        let fp = &rec.deps[p];
        if let Entry::Vacant(slot) = olds.entry(fp.clone()) {
            slot.insert(st.read_export(fp).with_context(|| format!("old export data of {p}"))?);
        }
    }
    let olds = &*olds;
    let mut pairs = Vec::with_capacity(changed.len());
    for p in changed {
        let (d, ofp, nfp) = (&prep.deps[p], rec.deps[p].as_str(), prep.fps[p].as_str());
        let ni = d.new_inliner()?;
        let old = Export::parse(&olds[ofp], p, ni)?;
        let new = Export::parse(d.bits()?, p, ni)?;
        if old.self_name != new.self_name || old.has_init != new.has_init {
            return Ok(Decision::blocked(BlockKind::PkgIdentity, format!("{p}: package name or init changed")));
        }
        if old.pkg_hash()? != new.pkg_hash()? {
            return Ok(Decision::blocked(BlockKind::ImportGraph, format!("{p}: import graph changed")));
        }
        let _ = st.write_export(nfp, d.bits()?);
        if let Some(names) = rec.roots.names.get(p) {
            let (mut oc, mut nc) = (HashCache::open(st, ofp), HashCache::open(st, nfp));
            let mut differs = None;
            for n in names {
                if oc.get(&old, p, n)? != nc.get(&new, p, n)? {
                    differs = Some(n);
                    break;
                }
            }
            oc.flush(st);
            nc.flush(st);
            if let Some(n) = differs {
                return Ok(Decision::blocked(BlockKind::UsesChanged, format!("uses changed {p}.{n}")));
            }
        }
        pairs.push((ofp, nfp, old, new));
    }
    let lsyms: Vec<(LsymTable, LsymTable)> =
        pairs.iter().map(|(ofp, nfp, old, new)| (lsyms_for(st, ofp, old), lsyms_for(st, nfp, new))).collect();
    let deps: Vec<patch::ChangedDep> = pairs
        .iter()
        .zip(&lsyms)
        .map(|((_, _, old, new), (ol, nl))| patch::ChangedDep { old, new, old_lsyms: ol, new_lsyms: nl })
        .collect();
    let new_fps: HashMap<String, [u8; 8]> = prep.deps.iter().map(|(k, d)| (k.clone(), d.fp)).collect();
    let old_out = st.read_blob(&rec.out)?;
    let target = patch::Target {
        old_archive: &old_out,
        self_path: &ca.pkg,
        old_buildid: &rec.buildid,
        new_buildid: &ca.buildid,
        full_dwarf: ca.full_dwarf,
    };
    let p = patch::patch(&target, &deps, &new_fps)?;
    Ok(Decision::Reuse { patched: p.bytes, remapped: p.remapped_refs, changed: deps.len() })
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
        Ok(Err(e)) => match e.downcast_ref::<patch::Guard>() {
            Some(g) => Decision::blocked(BlockKind::Guard, g.0.clone()),
            None => {
                let kind = if is_evicted(&e) { BlockKind::Evicted } else { BlockKind::PatchFailed };
                Decision::blocked(kind, format!("patch failed: {e:#}"))
            }
        },
        Err(_) => Decision::blocked(BlockKind::PatchPanicked, "patch panicked"),
    }
}

fn choose(st: &Store, ca: &CompileArgs, recs: &[Record], prep: &Prep) -> (Decision, usize) {
    let n = recs.len().min(store::max_records());
    let mut olds = HashMap::new();
    let mut best: Option<(usize, BlockKind, String)> = None;
    for (i, rec) in recs[..n].iter().enumerate() {
        let (changed, d) = match changed_deps(rec, &prep.fps) {
            None => (usize::MAX, Decision::blocked(BlockKind::ImportSet, "import set changed")),
            Some(c) => (c.len(), try_decide(st, ca, rec, prep, &c, &mut olds)),
        };
        match d {
            Decision::Blocked { kind, reason } => {
                if best.as_ref().is_none_or(|b| changed < b.0) {
                    best = Some((changed, kind, reason));
                }
            }
            d => return (d, i + 1),
        }
    }
    match best {
        None => (Decision::NoRecord, 0),
        Some((_, kind, r)) => {
            (Decision::blocked(kind, format!("{r} (tried {n} record{})", if n == 1 { "" } else { "s" })), n)
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

fn save_record(st: &Store, ca: &CompileArgs, prep: &Prep, recs: &[Record], roots: Roots, out: &[u8]) -> Result<()> {
    for (k, d) in &prep.deps {
        let fp = &prep.fps[k];
        if !st.has_export(fp) {
            st.write_export(fp, d.bits()?)?;
        }
    }
    let rec = Record {
        pkg: ca.pkg.clone(),
        buildid: ca.buildid.clone(),
        out: st.write_blob(out)?,
        deps: prep.fps.clone(),
        roots,
    };
    st.write_record(&prep.key, &rec, recs)
}

fn toolexec(tool: String, args: Vec<String>) -> Result<ExitCode> {
    let m = mode();
    let base = Path::new(&tool).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if m == Mode::Off || base != "compile" || args.iter().any(|a| a.starts_with("-V")) {
        return Err(exec_tool(&tool, &args));
    }
    let t0 = Instant::now();
    let (Ok(ca), Ok(st)) = (parse_args(&args), Store::open()) else {
        return Err(exec_tool(&tool, &args));
    };
    let mut ev = Event::new(&ca.pkg);

    let prep = match ca.unsupported {
        Some(u) => Err(Decision::Unsupported(u)),
        None => prepare(&st, &tool, &ca).map_err(|e| Decision::PrepareFailed(format!("prepare failed: {e:#}"))),
    };
    ev.prep_ms = ms(t0);
    let (prep, recs, decision, tried) = match prep {
        Ok(p) => {
            let recs = st.read_records(&p.key);
            let (d, tried) = choose(&st, &ca, &recs, &p);
            (Some(p), recs, d, tried)
        }
        Err(d) => (None, Vec::new(), d, 0),
    };
    ev.check_ms = ms(t0);
    ev.decide_ms = ev.check_ms - ev.prep_ms;

    ev.decision = decision.name().to_string();
    ev.tried = tried;
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
        Decision::Reuse { patched, remapped, changed } => {
            ev.record = tried - 1;
            ev.remapped = remapped;
            ev.changed_deps = changed;
            Some(patched)
        }
    };

    let output = match patched {
        Some(p) if m == Mode::Active && !sampled(&ca) => {
            std::fs::write(&ca.out, &p)?;
            p
        }
        patched => {
            if m == Mode::Active && patched.is_some() {
                ev.decision = "reuse+verify".into();
            }
            let t1 = Instant::now();
            let status = Command::new(&tool).args(&args).status()?;
            ev.compile_ms = ms(t1);
            ev.compiled = true;
            if !status.success() {
                ev.decision = format!("{}+compile-failed", ev.decision);
                st.log(&ev);
                return Ok(ExitCode::from(status.code().unwrap_or(1) as u8));
            }
            let real = std::fs::read(&ca.out)?;
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
        }
    };

    if let Some(p) = &prep {
        let tr = Instant::now();
        let roots = match recs.first() {
            Some(r) => Ok(r.roots.clone()),
            None => compute_roots(&ca, p),
        };
        if let Err(e) = roots.and_then(|roots| save_record(&st, &ca, p, &recs, roots, &output)) {
            ev.reason = format!("{} record failed: {e:#}", ev.reason);
        }
        ev.record_ms = ms(tr);
    }
    let tg = Instant::now();
    st.maybe_evict();
    ev.gc_ms = ms(tg);
    ev.total_ms = ms(t0);
    st.log(&ev);
    Ok(ExitCode::SUCCESS)
}

fn dumphash(archive: &str, pkg: &str) -> Result<()> {
    let b = std::fs::read(archive)?;
    let members = ar::parse(&b)?;
    let o = &ar::member(&members, "_go_.o").context("no _go_.o")?.data;
    let obj = goobj::Obj::parse(o)?;
    println!("defs {} hashed64 {} hashed {} nonpkgdef {} pkgs {:?}", obj.n_def, obj.n_h64, obj.n_hashed, obj.n_nonpkgdef, obj.pkg_index(o));
    for esc in [true, false] {
        let mut h = goobj::Hasher::new(&obj, o, pkg, esc);
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(|s| s.as_str()) {
        Some("stats") => store::stats(args.get(1).map(|s| s.as_str())).map(|_| ExitCode::SUCCESS),
        Some("gc") if args.get(1).is_some_and(|a| a == "--auto") => Store::open().map(|s| {
            s.auto_gc();
            ExitCode::SUCCESS
        }),
        Some("gc") => Store::open().and_then(|s| s.gc()).map(|(a, b)| {
            println!("store: {:.2} GiB -> {:.2} GiB", a as f64 / (1u64 << 30) as f64, b as f64 / (1u64 << 30) as f64);
            ExitCode::SUCCESS
        }),
        Some("clear-log") => Store::open().and_then(|s| s.clear_log()).map(|_| ExitCode::SUCCESS),
        Some("dumphash") if args.len() == 3 => dumphash(&args[1], &args[2]).map(|_| ExitCode::SUCCESS),
        Some(t) if t.contains('/') => toolexec(args[0].clone(), args[1..].to_vec()),
        _ => {
            eprintln!("usage: go build -toolexec=gocut ... | gocut stats [since-seconds] | gocut clear-log");
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
