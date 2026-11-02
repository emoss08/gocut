use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::usage::Roots;

#[derive(Serialize, Deserialize)]
pub struct Record {
    pub pkg: String,
    pub buildid: String,
    pub out: String,
    pub deps: BTreeMap<String, String>,
    pub roots: Roots,
}

pub type LsymTable = Vec<(String, String, Vec<i64>)>;

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn encode_lsyms(t: &LsymTable) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(t.len() as u32).to_le_bytes());
    for (p, n, v) in t {
        put_str(&mut out, p);
        put_str(&mut out, n);
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        for x in v {
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
    out
}

fn decode_lsyms(b: &[u8]) -> Option<LsymTable> {
    let mut i = 0usize;
    let u32_at = |i: &mut usize| -> Option<u32> {
        let v = u32::from_le_bytes(b.get(*i..*i + 4)?.try_into().ok()?);
        *i += 4;
        Some(v)
    };
    let n = u32_at(&mut i)? as usize;
    let mut t = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let l = u32_at(&mut i)? as usize;
        let p = std::str::from_utf8(b.get(i..i + l)?).ok()?.to_string();
        i += l;
        let l = u32_at(&mut i)? as usize;
        let nm = std::str::from_utf8(b.get(i..i + l)?).ok()?.to_string();
        i += l;
        let k = u32_at(&mut i)? as usize;
        let mut v = Vec::with_capacity(k.min(1 << 16));
        for _ in 0..k {
            v.push(i64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?));
            i += 8;
        }
        t.push((p, nm, v));
    }
    Some(t)
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Event {
    pub ts: f64,
    pub pkg: String,
    pub decision: String,
    pub reason: String,
    pub shadow: String,
    pub compiled: bool,
    pub check_ms: f64,
    pub compile_ms: f64,
    pub total_ms: f64,
    pub remapped: usize,
    pub changed_deps: usize,
    pub record: usize,
    pub prep_ms: f64,
    pub decide_ms: f64,
    pub record_ms: f64,
    pub gc_ms: f64,
    pub kind: String,
    pub tried: usize,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub phases: BTreeMap<String, f64>,
}

thread_local! {
    static PHASES: std::cell::RefCell<BTreeMap<String, f64>> = const { std::cell::RefCell::new(BTreeMap::new()) };
}

/// Runs `f`, adding its wall time in milliseconds to phase `name` of the current event.
pub fn timed<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let t = std::time::Instant::now();
    let r = f();
    add_phase(name, t.elapsed().as_secs_f64() * 1000.0);
    r
}

pub fn add_phase(name: &str, ms: f64) {
    PHASES.with(|p| *p.borrow_mut().entry(name.to_string()).or_default() += ms);
}

pub fn take_phases() -> BTreeMap<String, f64> {
    PHASES.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

impl Event {
    pub fn new(pkg: &str) -> Event {
        let ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        Event { ts, pkg: pkg.to_string(), ..Default::default() }
    }
}

pub fn env_or<T: FromStr>(k: &str, default: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn max_records() -> usize {
    env_or("GOCUT_RECORDS", 4usize).max(1)
}

fn parse_records(b: &[u8]) -> Vec<Record> {
    serde_json::from_slice::<Vec<Record>>(b)
        .or_else(|_| serde_json::from_slice::<Record>(b).map(|r| vec![r]))
        .unwrap_or_default()
}

fn touch(p: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(p) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn age(p: &Path) -> Option<Duration> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()?.elapsed().ok()
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), path.file_name().unwrap().to_string_lossy()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn append(p: &Path, data: &[u8]) -> Result<()> {
    std::fs::OpenOptions::new().create(true).append(true).open(p)?.write_all(data)?;
    Ok(())
}

fn read_zst(p: &Path, what: &str) -> Result<Vec<u8>> {
    let z = std::fs::read(p).with_context(|| format!("saved {what} was evicted"))?;
    touch(p);
    Ok(zstd::decode_all(z.as_slice())?)
}

struct Entry {
    path: PathBuf,
    len: u64,
    mtime: SystemTime,
}

fn walk_files(dir: &Path, out: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            walk_files(&e.path(), out);
        } else if let Ok(mtime) = md.modified() {
            out.push(Entry { path: e.path(), len: md.len(), mtime });
        }
    }
}

fn stem(p: &Path) -> Option<&str> {
    p.file_name()?.to_str()?.split('.').next()
}

struct RecFile {
    path: PathBuf,
    mtime: SystemTime,
    recs: Vec<Record>,
    keep: Vec<bool>,
}

fn load_rec_files(files: &[Entry], rec_dir: &Path) -> Vec<RecFile> {
    let mut out: Vec<RecFile> = files
        .iter()
        .filter(|f| f.path.starts_with(rec_dir) && f.path.extension().is_some_and(|e| e == "json"))
        .filter_map(|f| {
            let recs = parse_records(&std::fs::read(&f.path).ok()?);
            Some(RecFile { path: f.path.clone(), mtime: f.mtime, keep: vec![false; recs.len()], recs })
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.mtime));
    out
}

fn select_live(files: &[Entry], recfiles: &mut [RecFile], target: u64) -> HashSet<String> {
    let mut size_of: HashMap<&str, u64> = HashMap::new();
    for f in files {
        if let Some(s) = stem(&f.path) {
            *size_of.entry(s).or_default() += f.len;
        }
    }
    let depth = recfiles.iter().map(|r| r.recs.len()).max().unwrap_or(0);
    let mut live: HashSet<String> = HashSet::new();
    let mut used = 0u64;
    for pos in 0..depth {
        for rf in recfiles.iter_mut() {
            let Some(r) = rf.recs.get(pos) else { continue };
            let mut need = 0u64;
            let mut fresh: HashSet<&str> = HashSet::new();
            for id in std::iter::once(&r.out).chain(r.deps.values()) {
                if !live.contains(id) && fresh.insert(id.as_str()) {
                    need += size_of.get(id.as_str()).copied().unwrap_or(0);
                }
            }
            if used + need > target {
                continue;
            }
            used += need;
            live.extend(fresh.into_iter().map(str::to_string));
            rf.keep[pos] = true;
        }
    }
    live
}

fn rewrite_records(recfiles: Vec<RecFile>) -> Result<()> {
    for rf in recfiles {
        let kept: Vec<&Record> = rf.recs.iter().zip(&rf.keep).filter(|(_, k)| **k).map(|(r, _)| r).collect();
        if kept.is_empty() {
            let _ = std::fs::remove_file(&rf.path);
        } else {
            let _ = atomic_write(&rf.path, &serde_json::to_vec(&kept)?);
        }
    }
    Ok(())
}

fn delete_unreferenced(files: Vec<Entry>, rec_dir: &Path, live: &HashSet<String>) -> u64 {
    let mut now = 0u64;
    for f in files {
        if f.path.starts_with(rec_dir) || stem(&f.path).is_some_and(|s| live.contains(s)) {
            if f.path.exists() {
                now += f.len;
            }
            continue;
        }
        let _ = std::fs::remove_file(&f.path);
    }
    now
}

pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn open() -> Result<Store> {
        let dir = match std::env::var_os("GOCUT_DIR") {
            Some(d) => PathBuf::from(d),
            None => {
                let base = std::env::var_os("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
                    .context("no HOME")?;
                base.join("gocut")
            }
        };
        std::fs::create_dir_all(&dir)?;
        Ok(Store { dir })
    }

    fn shard(&self, sub: &str, id: &str, ext: &str) -> PathBuf {
        self.dir.join(sub).join(&id[..2]).join(format!("{id}.{ext}"))
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    fn added_path(&self) -> PathBuf {
        self.dir.join("gc.added")
    }

    fn total_path(&self) -> PathBuf {
        self.dir.join("gc.total")
    }

    // gc.added holds one decimal byte count per line written since the last walk; gc.total that walk's size.
    fn note_written(&self, n: usize) {
        let _ = append(&self.added_path(), format!("{n}\n").as_bytes());
    }

    fn write_once(&self, p: &Path, data: impl FnOnce() -> Result<Vec<u8>>) -> Result<()> {
        if !p.exists() {
            let d = data()?;
            atomic_write(p, &d)?;
            self.note_written(d.len());
        }
        Ok(())
    }

    pub fn read_records(&self, key: &str) -> Vec<Record> {
        let p = self.shard("rec", key, "json");
        let Ok(b) = std::fs::read(&p) else { return Vec::new() };
        touch(&p);
        parse_records(&b)
    }

    pub fn write_record(&self, key: &str, r: &Record, old: &[Record]) -> Result<()> {
        let kept = old.iter().filter(|o| o.deps != r.deps && o.out.len() >= 2 && self.blob_path(&o.out).exists());
        let recs: Vec<&Record> = std::iter::once(r).chain(kept).take(max_records()).collect();
        let data = serde_json::to_vec(&recs)?;
        atomic_write(&self.shard("rec", key, "json"), &data)?;
        self.note_written(data.len());
        Ok(())
    }

    fn blob_path(&self, h: &str) -> PathBuf {
        self.shard("out", h, "a.zst")
    }

    pub fn write_blob(&self, data: &[u8]) -> Result<String> {
        let h = blake3::hash(data).to_hex().to_string();
        self.write_once(&self.blob_path(&h), || Ok(zstd::bulk::compress(data, 1)?))?;
        Ok(h)
    }

    pub fn read_blob(&self, h: &str) -> Result<Vec<u8>> {
        anyhow::ensure!(h.len() >= 2, "saved output hash is malformed");
        let out = read_zst(&self.blob_path(h), "output")?;
        anyhow::ensure!(blake3::hash(&out).to_hex().as_str() == h, "saved output is corrupt");
        Ok(out)
    }

    fn export_path(&self, fp: &str) -> PathBuf {
        self.shard("ex", fp, "bits.zst")
    }

    pub fn has_export(&self, fp: &str) -> bool {
        self.export_path(fp).exists()
    }

    pub fn write_export(&self, fp: &str, bits: &[u8]) -> Result<()> {
        self.write_once(&self.export_path(fp), || Ok(zstd::bulk::compress(bits, 1)?))
    }

    pub fn read_export(&self, fp: &str) -> Result<Vec<u8>> {
        read_zst(&self.export_path(fp), "export data")
    }

    // <fp>.hash holds appended "name\thash\n" lines; later lines win and an unterminated tail is ignored.
    pub fn read_hashes(&self, fp: &str) -> HashMap<String, String> {
        let s = std::fs::read_to_string(self.shard("ex", fp, "hash")).unwrap_or_default();
        let complete = s.rfind('\n').map_or("", |i| &s[..i]);
        complete
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(_, h)| *h == "-" || h.len() == 64)
            .map(|(n, h)| (n.to_string(), h.to_string()))
            .collect()
    }

    pub fn append_hashes(&self, fp: &str, add: &[(String, String)]) -> Result<()> {
        if add.is_empty() {
            return Ok(());
        }
        let mut buf = String::new();
        for (n, h) in add {
            buf.push_str(n);
            buf.push('\t');
            buf.push_str(h);
            buf.push('\n');
        }
        let p = self.shard("ex", fp, "hash");
        std::fs::create_dir_all(p.parent().context("no parent")?)?;
        append(&p, buf.as_bytes())?;
        self.note_written(buf.len());
        Ok(())
    }

    // <fp>.same names the fingerprint whose export data hashes exactly like <fp>'s (positions and
    // symbol indices aside). Name hashes and package facts are then kept under that fingerprint.
    pub fn canonical(&self, fp: &str) -> String {
        match std::fs::read_to_string(self.shard("ex", fp, "same")) {
            Ok(c) if c.len() == fp.len() && c.bytes().all(|b| b.is_ascii_hexdigit()) => c,
            _ => fp.to_string(),
        }
    }

    pub fn link_same(&self, fp: &str, canonical: &str) -> Result<()> {
        if fp != canonical {
            self.write_once(&self.shard("ex", fp, "same"), || Ok(canonical.as_bytes().to_vec()))?;
        }
        Ok(())
    }

    // <fp>.pkg: "name\tinit\tpkghash", the facts the identity and import-graph checks compare.
    pub fn read_pkg_facts(&self, fp: &str) -> Option<(String, bool, String)> {
        let s = std::fs::read_to_string(self.shard("ex", fp, "pkg")).ok()?;
        let mut it = s.split('\t');
        let (n, i, h) = (it.next()?, it.next()?, it.next()?);
        (h.len() == 64 && it.next().is_none()).then(|| (n.to_string(), i == "1", h.to_string()))
    }

    pub fn write_pkg_facts(&self, fp: &str, f: &(String, bool, String)) -> Result<()> {
        let s = format!("{}\t{}\t{}", f.0, if f.1 { "1" } else { "0" }, f.2);
        self.write_once(&self.shard("ex", fp, "pkg"), || Ok(s.into_bytes()))
    }

    pub fn has_lsyms(&self, fp: &str) -> bool {
        self.shard("ex", fp, "lsym").exists()
    }

    pub fn read_lsyms(&self, fp: &str) -> Option<LsymTable> {
        decode_lsyms(&std::fs::read(self.shard("ex", fp, "lsym")).ok()?)
    }

    pub fn write_lsyms(&self, fp: &str, t: &LsymTable) -> Result<()> {
        self.write_once(&self.shard("ex", fp, "lsym"), || Ok(encode_lsyms(t)))
    }

    pub fn save_mismatch(&self, pkg: &str, patched: &[u8], real: &[u8]) {
        let safe = pkg.replace('/', "_");
        let d = self.dir.join("mismatch");
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(d.join(format!("{safe}.patched.a")), patched);
        let _ = std::fs::write(d.join(format!("{safe}.real.a")), real);
        self.note_written(patched.len() + real.len());
    }

    pub fn log(&self, ev: &Event) {
        let Ok(mut line) = serde_json::to_string(ev) else { return };
        line.push('\n');
        let _ = append(&self.log_path(), line.as_bytes());
    }

    pub fn clear_log(&self) -> Result<()> {
        let _ = std::fs::remove_file(self.log_path());
        Ok(())
    }

    fn rotate_log(&self) {
        let log = self.log_path();
        if std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0) > 64 << 20 {
            let _ = std::fs::rename(&log, self.dir.join("events.old.jsonl"));
        }
    }

    pub fn budget(&self) -> u64 {
        env_or("GOCUT_MAX_BYTES", 4u64 << 30)
    }

    fn gc_recent(&self) -> bool {
        age(&self.dir.join("gc.stamp")).is_some_and(|a| a.as_secs() < 20)
    }

    pub fn maybe_evict(&self) {
        if self.gc_recent() {
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let _ = Command::new(exe)
            .args(["gc", "--auto"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    pub fn auto_gc(&self) {
        if self.gc_recent() {
            return;
        }
        let lock = self.dir.join("gc.lock");
        if age(&lock).is_some_and(|a| a.as_secs() > 600) {
            let _ = std::fs::remove_file(&lock);
        }
        if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() {
            return;
        }
        let _ = std::fs::write(self.dir.join("gc.stamp"), b"");
        self.rotate_log();
        if !self.fits_budget() {
            let _ = self.walk();
        }
        let _ = std::fs::remove_file(&lock);
    }

    fn fits_budget(&self) -> bool {
        let Some(last) = std::fs::read_to_string(self.total_path()).ok().and_then(|s| s.trim().parse::<u64>().ok())
        else {
            return false;
        };
        let added: u64 = std::fs::read_to_string(self.added_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.parse::<u64>().ok())
            .sum();
        last.saturating_add(added) < self.budget()
    }

    pub fn gc(&self) -> Result<(u64, u64)> {
        self.rotate_log();
        self.walk()
    }

    fn walk(&self) -> Result<(u64, u64)> {
        let _ = std::fs::remove_file(self.added_path());
        let _ = std::fs::remove_file(self.total_path());
        let (total, now) = self.evict()?;
        let _ = std::fs::write(self.total_path(), now.to_string());
        Ok((total, now))
    }

    fn inventory(&self) -> Vec<Entry> {
        let mut files = Vec::new();
        for sub in ["rec", "out", "ex", "mismatch", "keys"] {
            walk_files(&self.dir.join(sub), &mut files);
        }
        files
    }

    fn evict(&self) -> Result<(u64, u64)> {
        let files = self.inventory();
        let total: u64 = files.iter().map(|f| f.len).sum();
        let budget = self.budget();
        if total <= budget {
            return Ok((total, total));
        }
        let rec_dir = self.dir.join("rec");
        let mut recfiles = load_rec_files(&files, &rec_dir);
        let live = select_live(&files, &mut recfiles, budget / 10 * 8);
        rewrite_records(recfiles)?;
        Ok((total, delete_unreferenced(files, &rec_dir, &live)))
    }
}

pub fn stats(since: Option<&str>) -> Result<()> {
    let st = Store::open()?;
    let data = std::fs::read_to_string(st.log_path()).unwrap_or_default();
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs_f64();
    let cutoff = since.and_then(|s| s.parse::<f64>().ok()).map(|s| now - s).unwrap_or(0.0);
    let mut by: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut shadow: BTreeMap<String, usize> = BTreeMap::new();
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let (mut check, mut compile, mut n) = (0.0, 0.0, 0usize);
    let mut saved_ms = 0.0;
    let mut older = 0usize;
    let mut reuse_cost: Vec<f64> = Vec::new();
    let mut phase_sum: BTreeMap<String, f64> = BTreeMap::new();
    for line in data.lines() {
        let Ok(ev) = serde_json::from_str::<Event>(line) else { continue };
        if ev.ts < cutoff {
            continue;
        }
        n += 1;
        let e = by.entry(ev.decision.clone()).or_default();
        e.0 += 1;
        e.1 += ev.compile_ms;
        if ev.decision.starts_with("reuse") && ev.record > 0 {
            older += 1;
        }
        if ev.decision.starts_with("reuse") && !ev.decision.contains("failed") {
            reuse_cost.push(ev.total_ms - ev.compile_ms);
            for (k, v) in &ev.phases {
                *phase_sum.entry(k.clone()).or_default() += v;
            }
        }
        if !ev.shadow.is_empty() {
            let k = if ev.shadow.starts_with("mismatch") { "mismatch".to_string() } else { ev.shadow.clone() };
            *shadow.entry(k).or_default() += 1;
            if ev.shadow == "match" {
                saved_ms += ev.compile_ms;
            }
        }
        if ev.decision == "blocked" || ev.decision == "unsupported" {
            let k = if ev.kind.is_empty() {
                ev.reason.split(':').next().unwrap_or("").chars().take(60).collect()
            } else {
                ev.kind.clone()
            };
            *reasons.entry(k).or_default() += 1;
        }
        check += ev.check_ms;
        compile += ev.compile_ms;
    }
    println!("compiles seen: {n}");
    for (k, (c, ms)) in &by {
        println!("  {k:<28} {c:>6}   compile time {:.1}s", ms / 1000.0);
    }
    if older > 0 {
        println!("  {:<28} {older:>6}", "reused from an older record");
    }
    if !shadow.is_empty() {
        println!("shadow verification:");
        for (k, c) in &shadow {
            println!("  {k:<28} {c:>6}");
        }
        println!("  compile time a verified reuse would have skipped: {:.1}s", saved_ms / 1000.0);
    }
    if !reasons.is_empty() {
        println!("why not reused:");
        let mut rs: Vec<_> = reasons.into_iter().collect();
        rs.sort_by_key(|r| std::cmp::Reverse(r.1));
        for (r, c) in rs.iter().take(15) {
            println!("  {c:>6}  {r}");
        }
    }
    println!("gocut overhead (check+patch): {:.1}s total; compile: {:.1}s total", check / 1000.0, compile / 1000.0);
    if !reuse_cost.is_empty() {
        reuse_cost.sort_by(f64::total_cmp);
        let q = |f: f64| reuse_cost[((reuse_cost.len() - 1) as f64 * f).round() as usize];
        println!(
            "gocut time per reused package (excluding compiles): median {:.0} ms, p90 {:.0} ms, mean {:.0} ms",
            q(0.5),
            q(0.9),
            reuse_cost.iter().sum::<f64>() / reuse_cost.len() as f64
        );
        let mut ps: Vec<_> = phase_sum.into_iter().collect();
        ps.sort_by(|a, b| b.1.total_cmp(&a.1));
        let line: Vec<String> =
            ps.iter().map(|(k, v)| format!("{k} {:.1}", v / reuse_cost.len() as f64)).collect();
        println!("  mean ms by phase: {}", line.join(", "));
    }
    Ok(())
}
