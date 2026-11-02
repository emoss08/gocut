use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::{Context, Result, bail, ensure};

use crate::pkgbits::{self, Elem, PkgBits, Rd, Reloc, Section};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Alias,
    Const,
    Type,
    Func,
    Var,
    Stub,
}

impl Tag {
    fn from_u64(v: u64) -> Result<Tag> {
        Ok(match v {
            0 => Tag::Alias,
            1 => Tag::Const,
            2 => Tag::Type,
            3 => Tag::Func,
            4 => Tag::Var,
            5 => Tag::Stub,
            _ => bail!("bad object tag {v}"),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ObjIdent {
    pub path: String,
    pub name: String,
    pub tag: Tag,
}

#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub start: usize,
    pub end: usize,
    pub value: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PosSlot {
    pub start: usize,
    pub end: usize,
    pub line: u64,
    pub col: u64,
}

const VERSION_V2: u32 = 2;
const POS_MASK: u8 = 0xfc;
const POS_RAW: &[u8] = b"\xfbraw";
const EXT_MASK: u8 = 0xfe;
const EXT_MASK_UNINDEXED: u8 = 0xfd;
const EXT_RAW: &[u8] = b"\xffraw";

enum ObjShape {
    Other,
    Func(usize),
    Type(Vec<usize>),
}

struct ObjInfo {
    shape: ObjShape,
    pos: Vec<PosSlot>,
}

// Raw(n): the element's own bytes after its n-byte reloc table.
enum Payload {
    Raw(usize),
    Owned(Vec<u8>),
}

struct Node {
    payload: Payload,
    children: Vec<(u8, Section, u32)>,
}

fn mask_ranges(raw: &[u8], ranges: impl IntoIterator<Item = (usize, usize, u8)>) -> Option<Vec<u8>> {
    let mut ranges = ranges.into_iter().peekable();
    ranges.peek()?;
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    for (start, end, marker) in ranges {
        out.extend_from_slice(&raw[at..start]);
        out.push(marker);
        at = end;
    }
    out.extend_from_slice(&raw[at..]);
    Some(out)
}

fn mask_pos(raw: &[u8], pos: &[PosSlot]) -> Option<Vec<u8>> {
    mask_ranges(raw, pos.iter().map(|s| (s.start, s.end, POS_MASK)))
}

fn tagged(prefix: &[u8], raw: &[u8]) -> Vec<u8> {
    [prefix, raw].concat()
}

fn slot(rd: &mut Rd, slots: &mut Vec<Slot>) -> Result<i64> {
    let start = rd.pos;
    let value = rd.varint()?;
    slots.push(Slot { start, end: rd.pos, value });
    Ok(value)
}

fn lsym_slot(rd: &mut Rd, slots: &mut Vec<Slot>) -> Result<()> {
    if slot(rd, slots)? < 0 {
        rd.skip_uvarint()?;
        rd.boolean()?;
    }
    Ok(())
}

pub struct Export<'a> {
    pub pb: PkgBits<'a>,
    pub self_path: String,
    pub self_name: String,
    pub has_init: bool,
    pub idents: Vec<ObjIdent>,
    self_pkg: u32,
    new_inliner: bool,
    by_ident: HashMap<String, HashMap<String, u32>>,
    bodies: HashMap<u32, Vec<u32>>,
    objs: RefCell<HashMap<u32, Rc<ObjInfo>>>,
    nodes: RefCell<HashMap<usize, Rc<Node>>>,
}

struct ER<'a, 'b> {
    pb: &'b PkgBits<'a>,
    relocs: Vec<Reloc>,
    rd: Rd<'a>,
    slots: Vec<PosSlot>,
}

impl<'a, 'b> ER<'a, 'b> {
    fn new(pb: &'b PkgBits<'a>, k: Section, idx: u32) -> Result<ER<'a, 'b>> {
        Ok(ER::of(pb, pb.elem(k, idx)?))
    }

    fn of(pb: &'b PkgBits<'a>, e: Elem<'a>) -> ER<'a, 'b> {
        ER { pb, relocs: e.relocs, rd: Rd::new(e.payload), slots: Vec::new() }
    }

    fn reloc(&mut self, want: Section) -> Result<u32> {
        let i = self.rd.uvarint()? as usize;
        let r = self.relocs.get(i).context("reloc index out of range")?;
        ensure!(r.kind == want, "reloc kind {:?}, want {want:?}", r.kind);
        Ok(r.idx)
    }

    fn string(&mut self) -> Result<&'a [u8]> {
        let idx = self.reloc(Section::String)?;
        self.pb.string(idx)
    }

    // writer.pos: Bool(known), then Reloc(PosBase), Uint(line), Uint(col) back to back.
    fn pos(&mut self) -> Result<()> {
        if self.rd.boolean()? {
            self.reloc(Section::PosBase)?;
            let start = self.rd.pos;
            let line = self.rd.uvarint()?;
            let col = self.rd.uvarint()?;
            self.slots.push(PosSlot { start, end: self.rd.pos, line, col });
        }
        Ok(())
    }

    // localIdent and selector share one encoding: Reloc(Pkg), then Reloc(String).
    fn ident(&mut self) -> Result<()> {
        self.reloc(Section::Pkg)?;
        self.reloc(Section::String)?;
        Ok(())
    }

    // Derived types are an index into the ObjDict's derived list, not a reloc.
    fn typ_info(&mut self) -> Result<()> {
        if self.rd.boolean()? {
            self.rd.skip_uvarint()
        } else {
            self.reloc(Section::Type).map(|_| ())
        }
    }

    fn obj_info(&mut self) -> Result<()> {
        if self.pb.version < VERSION_V2 {
            ensure!(!self.rd.boolean()?, "derived func instance in objInfo");
        }
        self.reloc(Section::Obj)?;
        let n = self.rd.uvarint()?;
        for _ in 0..n {
            self.typ_info()?;
        }
        Ok(())
    }

    fn big_int(&mut self) -> Result<()> {
        self.reloc(Section::String)?;
        self.rd.boolean()?;
        Ok(())
    }

    fn scalar(&mut self) -> Result<()> {
        match self.rd.uvarint()? {
            0 => {
                self.rd.boolean()?;
            }
            1 | 5 => {
                self.reloc(Section::String)?;
            }
            2 => {
                self.rd.varint()?;
            }
            3 => self.big_int()?,
            4 => {
                self.big_int()?;
                self.big_int()?;
            }
            c => bail!("bad constant value code {c}"),
        }
        Ok(())
    }

    fn value(&mut self) -> Result<()> {
        let parts = if self.rd.boolean()? { 2 } else { 1 };
        for _ in 0..parts {
            self.scalar()?;
        }
        Ok(())
    }

    fn struct_type(&mut self) -> Result<()> {
        let n = self.rd.uvarint()?;
        for _ in 0..n {
            self.pos()?;
            self.ident()?;
            self.typ_info()?;
            self.reloc(Section::String)?;
            self.rd.boolean()?;
        }
        Ok(())
    }

    // The implicit flag is written only when there are 0 methods and exactly 1 embedded.
    fn interface_type(&mut self) -> Result<()> {
        let nm = self.rd.uvarint()?;
        let ne = self.rd.uvarint()?;
        if nm == 0 && ne == 1 {
            self.rd.boolean()?;
        }
        for _ in 0..nm {
            self.pos()?;
            self.ident()?;
            self.signature()?;
        }
        for _ in 0..ne {
            self.typ_info()?;
        }
        Ok(())
    }

    fn union_type(&mut self) -> Result<()> {
        let n = self.rd.uvarint()?;
        for _ in 0..n {
            self.rd.boolean()?;
            self.typ_info()?;
        }
        Ok(())
    }

    fn do_typ(&mut self) -> Result<()> {
        match self.rd.uvarint()? {
            0 | 11 => self.rd.skip_uvarint()?,
            1 => self.obj_info()?,
            2 | 3 => self.typ_info()?,
            4 | 5 => {
                self.rd.skip_uvarint()?;
                self.typ_info()?;
            }
            6 => {
                self.typ_info()?;
                self.typ_info()?;
            }
            7 => {
                self.signature()?;
            }
            8 => self.struct_type()?,
            9 => self.interface_type()?,
            10 => self.union_type()?,
            c => bail!("bad type code {c}"),
        }
        Ok(())
    }

    fn param(&mut self) -> Result<()> {
        self.pos()?;
        self.ident()?;
        self.typ_info()
    }

    fn params(&mut self) -> Result<usize> {
        let n = self.rd.uvarint()? as usize;
        for _ in 0..n {
            self.param()?;
        }
        Ok(n)
    }

    fn signature(&mut self) -> Result<usize> {
        let n = self.params()?;
        self.params()?;
        self.rd.boolean()?;
        Ok(n)
    }

    fn type_param_names(&mut self, n: usize) -> Result<()> {
        for _ in 0..n {
            self.pos()?;
            self.ident()?;
        }
        Ok(())
    }
}

fn utf8(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn body_base(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("(*") {
        return rest.split(").").next().unwrap_or(rest);
    }
    let base = name.split('.').next().unwrap_or(name);
    base.split('[').next().unwrap_or(base)
}

fn root_pkg(pb: &PkgBits) -> Result<u32> {
    ER::new(pb, Section::Meta, 0)?.reloc(Section::Pkg)
}

fn path_name(pb: &PkgBits, self_path: &str, pkg_idx: u32) -> Result<(String, String)> {
    let mut r = ER::new(pb, Section::Pkg, pkg_idx)?;
    let raw = r.string()?;
    let path = if raw.is_empty() { self_path.to_string() } else { utf8(raw) };
    if path == "builtin" || path == "unsafe" {
        return Ok((path.clone(), path));
    }
    let name = utf8(r.string()?);
    Ok((path, name))
}

fn read_ident(pb: &PkgBits, self_path: &str, i: u32, paths: &mut HashMap<u32, String>) -> Result<ObjIdent> {
    let mut r = ER::new(pb, Section::Name, i)?;
    let pkg = r.reloc(Section::Pkg)?;
    let name = utf8(r.string()?);
    let tag = Tag::from_u64(r.rd.uvarint()?)?;
    let path = match paths.get(&pkg) {
        Some(p) => p.clone(),
        None => {
            let p = path_name(pb, self_path, pkg)?.0;
            paths.insert(pkg, p.clone());
            p
        }
    };
    Ok(ObjIdent { path, name, tag })
}

fn type_pos(mut r: ER, idx: u32) -> Result<Vec<PosSlot>> {
    r.do_typ()?;
    ensure!(r.rd.at_end(), "trailing bytes in Type element {idx}");
    Ok(r.slots)
}

pub fn pkg_name(bits: &[u8]) -> Result<String> {
    let pb = PkgBits::parse(bits)?;
    Ok(path_name(&pb, "", root_pkg(&pb)?)?.1)
}

impl<'a> Export<'a> {
    pub fn parse(bits: &'a [u8], self_path: &str, new_inliner: bool) -> Result<Export<'a>> {
        let pb = PkgBits::parse(bits)?;
        let self_pkg = root_pkg(&pb)?;
        let self_name = path_name(&pb, self_path, self_pkg)?.1;

        let n = pb.num(Section::Name);
        let mut idents = Vec::with_capacity(n);
        let mut by_ident: HashMap<String, HashMap<String, u32>> = HashMap::new();
        let mut paths = HashMap::new();
        for i in 0..n as u32 {
            let id = read_ident(&pb, self_path, i, &mut paths)?;
            match by_ident.get_mut(&id.path) {
                Some(names) => {
                    names.insert(id.name.clone(), i);
                }
                None => {
                    by_ident.insert(id.path.clone(), HashMap::from([(id.name.clone(), i)]));
                }
            }
            idents.push(id);
        }

        let mut pr = ER::new(&pb, Section::Meta, 1)?;
        let has_init = pr.rd.boolean()?;
        let nb = pr.rd.uvarint()?;
        let mut by_base: HashMap<String, HashMap<String, Vec<u32>>> = HashMap::new();
        for _ in 0..nb {
            let path = pr.string()?;
            let path = if path.is_empty() { self_path.to_string() } else { utf8(path) };
            let name = utf8(pr.string()?);
            let body = pr.reloc(Section::Body)?;
            by_base.entry(path).or_default().entry(body_base(&name).to_string()).or_default().push(body);
        }
        let mut bodies = HashMap::new();
        if !by_base.is_empty() {
            for (i, id) in idents.iter().enumerate() {
                if let Some(bs) = by_base.get(&id.path).and_then(|names| names.get(&id.name)) {
                    bodies.insert(i as u32, bs.clone());
                }
            }
        }

        Ok(Export {
            pb,
            self_path: self_path.to_string(),
            self_name,
            has_init,
            idents,
            self_pkg,
            new_inliner,
            by_ident,
            bodies,
            objs: RefCell::default(),
            nodes: RefCell::default(),
        })
    }

    fn dict_counts(&self, i: u32) -> Result<(usize, usize, usize)> {
        let mut r = ER::new(&self.pb, Section::ObjDict, i)?;
        let implicits = r.rd.uvarint()? as usize;
        let receivers =
            if self.pb.version >= pkgbits::VERSION_GENERIC_METHODS { r.rd.uvarint()? as usize } else { 0 };
        let explicits = r.rd.uvarint()? as usize;
        Ok((implicits, receivers, explicits))
    }

    fn parse_obj(&self, i: u32) -> Result<ObjInfo> {
        let id = self.idents.get(i as usize).context("object index out of range")?;
        let generic_methods = self.pb.version >= pkgbits::VERSION_GENERIC_METHODS;
        let mut r = ER::new(&self.pb, Section::Obj, i)?;
        let shape = match id.tag {
            Tag::Stub => ObjShape::Other,
            Tag::Alias => {
                let (_, _, explicits) = self.dict_counts(i)?;
                r.pos()?;
                if self.pb.version >= VERSION_V2 {
                    r.type_param_names(explicits)?;
                }
                r.typ_info()?;
                ObjShape::Other
            }
            Tag::Const => {
                r.pos()?;
                r.typ_info()?;
                r.value()?;
                ObjShape::Other
            }
            Tag::Var => {
                r.pos()?;
                r.typ_info()?;
                ObjShape::Other
            }
            Tag::Func => {
                let (_, receivers, explicits) = self.dict_counts(i)?;
                r.pos()?;
                let mut recv = 0;
                if generic_methods && r.rd.boolean()? {
                    r.ident()?;
                    r.type_param_names(receivers)?;
                    r.param()?;
                    recv = 1;
                }
                r.type_param_names(explicits)?;
                let n = r.signature()?;
                r.pos()?;
                ObjShape::Func(recv + n)
            }
            Tag::Type => {
                let (_, _, explicits) = self.dict_counts(i)?;
                r.pos()?;
                r.type_param_names(explicits)?;
                r.typ_info()?;
                let nm = r.rd.uvarint()? as usize;
                let mut methods = Vec::with_capacity(nm.min(1024));
                for _ in 0..nm {
                    r.pos()?;
                    r.ident()?;
                    r.type_param_names(explicits)?;
                    r.param()?;
                    let n = r.signature()?;
                    r.pos()?;
                    methods.push(1 + n);
                }
                if generic_methods {
                    let ng = r.rd.uvarint()?;
                    for _ in 0..ng {
                        r.reloc(Section::Obj)?;
                    }
                }
                ObjShape::Type(methods)
            }
        };
        ensure!(r.rd.at_end(), "trailing bytes in Obj for {}.{}", id.path, id.name);
        Ok(ObjInfo { shape, pos: r.slots })
    }

    fn obj(&self, i: u32) -> Result<Rc<ObjInfo>> {
        if let Some(o) = self.objs.borrow().get(&i) {
            return Ok(Rc::clone(o));
        }
        let o = Rc::new(self.parse_obj(i)?);
        self.objs.borrow_mut().insert(i, Rc::clone(&o));
        Ok(o)
    }

    pub fn pos_slots(&self, k: Section, idx: u32) -> Result<Vec<PosSlot>> {
        match k {
            Section::Obj => Ok(self.obj(idx)?.pos.clone()),
            Section::Type => type_pos(ER::new(&self.pb, k, idx)?, idx),
            // A //line base records the directive's own position, then the line and column it sets.
            Section::PosBase => {
                let mut r = ER::new(&self.pb, k, idx)?;
                r.reloc(Section::String)?;
                if !r.rd.boolean()? {
                    r.pos()?;
                    r.rd.skip_uvarint()?;
                    r.rd.skip_uvarint()?;
                }
                ensure!(r.rd.at_end(), "trailing bytes in PosBase element {idx}");
                Ok(r.slots)
            }
            Section::String | Section::Meta | Section::Pkg | Section::Name | Section::ObjExt | Section::ObjDict => {
                Ok(Vec::new())
            }
            Section::Body => bail!("positions in Body elements are not supported"),
        }
    }

    fn func_ext(&self, rd: &mut Rd, nparams: usize, slots: &mut Vec<Slot>) -> Result<()> {
        rd.varint()?;
        lsym_slot(rd, slots)?;
        if rd.boolean()? {
            rd.skip_uvarint()?;
            for _ in 0..nparams {
                rd.skip_uvarint()?;
            }
            if rd.boolean()? {
                rd.skip_uvarint()?;
                rd.boolean()?;
                if self.new_inliner {
                    rd.skip_uvarint()?;
                }
            }
        } else {
            rd.skip_uvarint()?;
        }
        Ok(())
    }

    pub fn ext_slots(&self, i: u32) -> Result<Vec<Slot>> {
        let e = self.pb.elem(Section::ObjExt, i)?;
        self.ext_slots_in(i, e.payload)
    }

    fn ext_slots_in(&self, i: u32, payload: &[u8]) -> Result<Vec<Slot>> {
        let id = self.idents.get(i as usize).context("object index out of range")?;
        let mut rd = Rd::new(payload);
        let mut slots = Vec::new();
        match id.tag {
            Tag::Func | Tag::Type => match &self.obj(i)?.shape {
                ObjShape::Func(n) => self.func_ext(&mut rd, *n, &mut slots)?,
                ObjShape::Type(methods) => {
                    rd.varint()?;
                    slot(&mut rd, &mut slots)?;
                    slot(&mut rd, &mut slots)?;
                    for &n in methods {
                        self.func_ext(&mut rd, n, &mut slots)?;
                    }
                }
                ObjShape::Other => bail!("Obj {i} has no extension shape"),
            },
            Tag::Var => lsym_slot(&mut rd, &mut slots)?,
            Tag::Alias | Tag::Const | Tag::Stub => {}
        }
        ensure!(rd.at_end(), "trailing bytes in ObjExt for {}.{}", id.path, id.name);
        Ok(slots)
    }

    pub fn lsyms(&self, i: u32) -> Result<Vec<i64>> {
        Ok(self.ext_slots(i)?.into_iter().map(|s| s.value).collect())
    }

    fn masked(&self, k: Section, idx: u32, e: Elem<'a>) -> Option<Vec<u8>> {
        let raw = e.payload;
        match k {
            Section::ObjExt => match self.ext_slots_in(idx, raw) {
                Ok(slots) => mask_ranges(
                    raw,
                    slots.iter().map(|s| (s.start, s.end, if s.value >= 0 { EXT_MASK } else { EXT_MASK_UNINDEXED })),
                ),
                Err(_) => Some(tagged(EXT_RAW, raw)),
            },
            Section::Obj => match self.obj(idx) {
                Ok(o) => mask_pos(raw, &o.pos),
                Err(_) => Some(tagged(POS_RAW, raw)),
            },
            Section::Type => match type_pos(ER::of(&self.pb, e), idx) {
                Ok(pos) => mask_pos(raw, &pos),
                Err(_) => Some(tagged(POS_RAW, raw)),
            },
            _ => None,
        }
    }

    pub fn masked_payload(&self, k: Section, idx: u32) -> Result<Vec<u8>> {
        if k == Section::String {
            return Ok(self.pb.string(idx)?.to_vec());
        }
        let e = self.pb.elem(k, idx)?;
        let raw = e.payload;
        Ok(self.masked(k, idx, e).unwrap_or_else(|| raw.to_vec()))
    }

    // Importers read only path, name and imports of a Pkg element; its reloc table shape depends on link order.
    fn pkg_node(&self, idx: u32) -> Result<Node> {
        let mut r = ER::new(&self.pb, Section::Pkg, idx)?;
        let raw = r.string()?;
        let path: &[u8] = if raw.is_empty() { self.self_path.as_bytes() } else { raw };
        let mut out = Vec::with_capacity(path.len() + 32);
        out.extend_from_slice(&(path.len() as u32).to_le_bytes());
        out.extend_from_slice(path);
        if path == b"builtin" || path == b"unsafe" {
            return Ok(Node { payload: Payload::Owned(out), children: Vec::new() });
        }
        let name = r.string()?;
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name);
        let n = r.rd.uvarint()? as usize;
        let mut children = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            children.push((Section::Pkg as u8, Section::Pkg, r.reloc(Section::Pkg)?));
        }
        ensure!(r.rd.at_end(), "trailing bytes in Pkg element {idx}");
        Ok(Node { payload: Payload::Owned(out), children })
    }

    fn elem_node(&self, k: Section, idx: u32) -> Result<Node> {
        let raw = self.pb.raw(k, idx)?;
        let e = pkgbits::decode_elem(raw)?;
        let skip = raw.len() - e.payload.len();
        let mut children: Vec<(u8, Section, u32)> = e.relocs.iter().map(|r| (r.kind as u8, r.kind, r.idx)).collect();
        if k == Section::Obj {
            ensure!((idx as usize) < self.idents.len(), "object index out of range");
            children.extend([(0xa0, Section::Name, idx), (0xa1, Section::ObjExt, idx), (0xa2, Section::ObjDict, idx)]);
            if let Some(bodies) = self.bodies.get(&idx) {
                children.extend(bodies.iter().map(|&b| (0xa3, Section::Body, b)));
            }
        }
        let payload = match self.masked(k, idx, e) {
            Some(v) => Payload::Owned(v),
            None => Payload::Raw(skip),
        };
        Ok(Node { payload, children })
    }

    fn node(&self, k: Section, idx: u32) -> Result<Rc<Node>> {
        let a = self.pb.abs(k, idx)?;
        if let Some(n) = self.nodes.borrow().get(&a) {
            return Ok(Rc::clone(n));
        }
        let n = Rc::new(if k == Section::Pkg { self.pkg_node(idx)? } else { self.elem_node(k, idx)? });
        self.nodes.borrow_mut().insert(a, Rc::clone(&n));
        Ok(n)
    }

    fn enter(
        &self,
        k: Section,
        idx: u32,
        h: &mut blake3::Hasher,
        ord: &mut HashMap<(Section, u32), u32>,
    ) -> Result<Option<Rc<Node>>> {
        if let Some(o) = ord.get(&(k, idx)) {
            h.update(&[1]);
            h.update(&o.to_le_bytes());
            return Ok(None);
        }
        let n = ord.len() as u32;
        ord.insert((k, idx), n);
        h.update(&[2, k as u8]);
        if k == Section::String {
            let s = self.pb.string(idx)?;
            h.update(&(s.len() as u32).to_le_bytes());
            h.update(s);
            return Ok(None);
        }
        let node = self.node(k, idx)?;
        let payload = match &node.payload {
            Payload::Raw(skip) => &self.pb.raw(k, idx)?[*skip..],
            Payload::Owned(v) => v.as_slice(),
        };
        h.update(&(payload.len() as u32).to_le_bytes());
        h.update(payload);
        h.update(&(node.children.len() as u32).to_le_bytes());
        Ok(Some(node))
    }

    fn closure_hash(&self, k0: Section, i0: u32) -> Result<[u8; 32]> {
        struct Frame {
            node: Rc<Node>,
            next: usize,
        }
        let mut h = blake3::Hasher::new();
        let mut ord: HashMap<(Section, u32), u32> = HashMap::new();
        let mut stack: Vec<Frame> = Vec::new();
        if let Some(node) = self.enter(k0, i0, &mut h, &mut ord)? {
            stack.push(Frame { node, next: 0 });
        }
        while let Some(top) = stack.last_mut() {
            if top.next >= top.node.children.len() {
                stack.pop();
                continue;
            }
            let (tagb, k, idx) = top.node.children[top.next];
            top.next += 1;
            h.update(&[tagb]);
            if let Some(node) = self.enter(k, idx, &mut h, &mut ord)? {
                stack.push(Frame { node, next: 0 });
            }
        }
        Ok(*h.finalize().as_bytes())
    }

    pub fn obj_hash(&self, root: u32) -> Result<[u8; 32]> {
        self.closure_hash(Section::Obj, root)
    }

    pub fn pkg_hash(&self) -> Result<[u8; 32]> {
        self.closure_hash(Section::Pkg, self.self_pkg)
    }

    pub fn find(&self, path: &str, name: &str) -> Option<u32> {
        self.by_ident.get(path)?.get(name).copied()
    }
}

pub struct ExportFile {
    pub bits_start: usize,
    pub bits_end: usize,
    pub new_inliner: bool,
}

pub fn locate(pkgdef: &[u8]) -> Result<ExportFile> {
    let marker = b"\n$$B\nu";
    let start = find_bytes(pkgdef, marker).context("no unified export data marker")? + marker.len();
    let tail = b"\n$$\n";
    ensure!(pkgdef.ends_with(tail), "export data missing trailer");
    let end = pkgdef.len() - tail.len();
    let header_end = pkgdef.iter().position(|&b| b == b'\n').unwrap_or(0);
    let hs = String::from_utf8_lossy(&pkgdef[..header_end]);
    // Header is "go object GOOS GOARCH VERSION[ K=V] X:exp,exp"; wasm FuncExts carry three extra strings.
    let mut words = hs.split(' ');
    ensure!(words.next() == Some("go") && words.next() == Some("object"), "export data header is not a go object header");
    ensure!(words.nth(1) != Some("wasm"), "wasm export data is not supported");
    let new_inliner = hs.rsplit_once(" X:").is_some_and(|(_, x)| x.trim_end().split(',').any(|e| e == "newinliner"));
    Ok(ExportFile { bits_start: start, bits_end: end, new_inliner })
}

pub fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgbits::{NUM_SECTIONS, dump, encode_elem, put_uvarint};

    fn slot_tuples(v: Vec<PosSlot>) -> Vec<(usize, usize, u64, u64)> {
        v.into_iter().map(|p| (p.start, p.end, p.line, p.col)).collect()
    }

    const PATH: &str = "example.com/p";

    fn el(relocs: &[(Section, u32)], payload: &[u8]) -> Vec<u8> {
        let relocs: Vec<Reloc> = relocs.iter().map(|&(kind, idx)| Reloc { kind, idx }).collect();
        let mut out = Vec::new();
        encode_elem(&relocs, payload, &mut out);
        out
    }

    fn pos(out: &mut Vec<u8>, line: u64, col: u64) {
        out.extend_from_slice(&[1, 0]);
        put_uvarint(out, line);
        put_uvarint(out, col);
    }

    // Var F of type struct{ F <basic kind> }, and const C with a complex (big int, int64 -4) value.
    fn build(line: u64, kind: u8) -> Vec<u8> {
        let mut field = vec![8, 1];
        pos(&mut field, line, 5);
        field.extend_from_slice(&[1, 2, 0, 3, 4, 0]);
        let mut var = Vec::new();
        pos(&mut var, line + 1, 2);
        var.extend_from_slice(&[0, 1]);
        let mut cnst = Vec::new();
        pos(&mut cnst, line + 2, 7);
        cnst.extend_from_slice(&[0, 1, 1, 3, 2, 0, 2, 7]);
        let field_relocs = [
            (Section::PosBase, 0),
            (Section::Pkg, 0),
            (Section::String, 2),
            (Section::Type, 0),
            (Section::String, 0),
        ];
        let sections: [Vec<Vec<u8>>; NUM_SECTIONS] = [
            vec![b"".to_vec(), b"p".to_vec(), b"F".to_vec(), b"f.go".to_vec(), b"C".to_vec()],
            vec![el(&[(Section::Pkg, 0)], &[0]), el(&[], &[0, 0])],
            vec![el(&[(Section::String, 3)], &[0, 1])],
            vec![el(&[(Section::String, 0), (Section::String, 1)], &[0, 1, 0])],
            vec![
                el(&[(Section::Pkg, 0), (Section::String, 2)], &[0, 1, 4]),
                el(&[(Section::Pkg, 0), (Section::String, 4)], &[0, 1, 1]),
            ],
            vec![el(&[], &[0, kind]), el(&field_relocs, &field)],
            vec![
                el(&[(Section::PosBase, 0), (Section::Type, 1)], &var),
                el(&[(Section::PosBase, 0), (Section::Type, 0), (Section::String, 4)], &cnst),
            ],
            vec![el(&[(Section::String, 0)], &[1, 0, 0]), el(&[], &[])],
            vec![el(&[], &[0; 8]), el(&[], &[0; 8])],
            vec![],
        ];
        let mut ends = [0u32; NUM_SECTIONS];
        let mut sum = 0;
        for (e, s) in ends.iter_mut().zip(&sections) {
            sum += s.len() as u32;
            *e = sum;
        }
        let elems: Vec<Vec<u8>> = sections.into_iter().flatten().collect();
        dump(pkgbits::VERSION_GENERIC_METHODS, 0, &ends, &elems)
    }

    #[test]
    fn type_positions() {
        let bits = build(300, 2);
        let ex = Export::parse(&bits, PATH, false).unwrap();
        assert_eq!(slot_tuples(ex.pos_slots(Section::Type, 1).unwrap()), vec![(4, 7, 300, 5)]);
        assert!(ex.pos_slots(Section::Type, 0).unwrap().is_empty());
        assert_eq!(ex.masked_payload(Section::Type, 1).unwrap(), vec![8, 1, 1, 0, POS_MASK, 1, 2, 0, 3, 4, 0]);
    }

    #[test]
    fn obj_positions() {
        let bits = build(300, 2);
        let ex = Export::parse(&bits, PATH, false).unwrap();
        let f = ex.find(PATH, "F").unwrap();
        let c = ex.find(PATH, "C").unwrap();
        assert_eq!(slot_tuples(ex.pos_slots(Section::Obj, f).unwrap()), vec![(2, 5, 301, 2)]);
        assert_eq!(slot_tuples(ex.pos_slots(Section::Obj, c).unwrap()), vec![(2, 5, 302, 7)]);
        assert!(ex.pos_slots(Section::ObjDict, c).unwrap().is_empty());
        assert!(ex.pos_slots(Section::PosBase, 0).unwrap().is_empty());
        assert!(ex.pos_slots(Section::Body, 0).is_err());
    }

    #[test]
    fn hash_ignores_positions() {
        let (a, b, c) = (build(300, 2), build(7, 2), build(300, 3));
        let ea = Export::parse(&a, PATH, false).unwrap();
        let eb = Export::parse(&b, PATH, false).unwrap();
        let ec = Export::parse(&c, PATH, false).unwrap();
        for name in ["F", "C"] {
            let i = ea.find(PATH, name).unwrap();
            assert_eq!(ea.obj_hash(i).unwrap(), eb.obj_hash(i).unwrap(), "{name}");
        }
        let f = ea.find(PATH, "F").unwrap();
        assert_ne!(ea.obj_hash(f).unwrap(), ec.obj_hash(f).unwrap());
    }

    #[test]
    fn memo_keeps_hashes() {
        let bits = build(300, 2);
        let a = Export::parse(&bits, PATH, false).unwrap();
        let b = Export::parse(&bits, PATH, false).unwrap();
        let (f, c) = (a.find(PATH, "F").unwrap(), a.find(PATH, "C").unwrap());
        let (af, ac) = (a.obj_hash(f).unwrap(), a.obj_hash(c).unwrap());
        assert_eq!(b.obj_hash(c).unwrap(), ac);
        assert_eq!(b.obj_hash(f).unwrap(), af);
        assert_eq!(a.obj_hash(f).unwrap(), af);
        assert_eq!(a.pkg_hash().unwrap(), b.pkg_hash().unwrap());
        assert_eq!(a.masked_payload(Section::Obj, f).unwrap(), b.masked_payload(Section::Obj, f).unwrap());
    }

    #[test]
    fn package_name() {
        let bits = build(300, 2);
        assert_eq!(pkg_name(&bits).unwrap(), "p");
        assert_eq!(Export::parse(&bits, PATH, false).unwrap().self_name, "p");
        assert_eq!(Export::parse(&bits, PATH, false).unwrap().find(PATH, "X"), None);
    }
}
