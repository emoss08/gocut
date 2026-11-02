use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub struct Import {
    pub name: Option<String>,
    pub path: String,
}

#[derive(Default, Debug)]
pub struct FileUse {
    pub imports: Vec<Import>,
    pub selectors: BTreeSet<(String, String)>,
}

#[derive(Debug)]
enum Tok {
    Ident(String),
    Str(String),
    Dot,
    LParen,
    RParen,
    Semi,
    Other,
}

fn lex(src: &[u8]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = src.len();
    while i < n {
        let c = src[i];
        if c == b'\n' {
            out.push(Tok::Semi);
            i += 1;
        } else if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'/' && src.get(i + 1) == Some(&b'/') {
            while i < n && src[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && src.get(i + 1) == Some(&b'*') {
            i += 2;
            let mut nl = false;
            while i + 1 < n && !(src[i] == b'*' && src[i + 1] == b'/') {
                nl |= src[i] == b'\n';
                i += 1;
            }
            i += 2;
            if nl {
                out.push(Tok::Semi);
            }
        } else if c == b'"' {
            let mut s = Vec::new();
            i += 1;
            while i < n && src[i] != b'"' && src[i] != b'\n' {
                if src[i] == b'\\' && i + 1 < n {
                    s.push(src[i]);
                    i += 1;
                }
                s.push(src[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(String::from_utf8_lossy(&s).into_owned()));
        } else if c == b'`' {
            let mut s = Vec::new();
            i += 1;
            while i < n && src[i] != b'`' {
                s.push(src[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(String::from_utf8_lossy(&s).into_owned()));
        } else if c == b'\'' {
            i += 1;
            while i < n && src[i] != b'\'' && src[i] != b'\n' {
                if src[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Other);
        } else if c == b'_' || c.is_ascii_alphabetic() || c >= 0x80 {
            let s = i;
            while i < n && (src[i] == b'_' || src[i].is_ascii_alphanumeric() || src[i] >= 0x80) {
                i += 1;
            }
            out.push(Tok::Ident(String::from_utf8_lossy(&src[s..i]).into_owned()));
        } else if c.is_ascii_digit() || (c == b'.' && src.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            i += 1;
            while i < n && (src[i].is_ascii_alphanumeric() || src[i] == b'_' || src[i] == b'.') {
                if matches!(src[i], b'e' | b'E' | b'p' | b'P') && matches!(src.get(i + 1), Some(b'+' | b'-')) {
                    i += 1;
                }
                i += 1;
            }
            out.push(Tok::Other);
        } else if c == b'.' {
            if src.get(i + 1..i + 3) == Some(b"..".as_slice()) {
                i += 3;
                out.push(Tok::Other);
            } else {
                i += 1;
                out.push(Tok::Dot);
            }
        } else if c == b'(' {
            i += 1;
            out.push(Tok::LParen);
        } else if c == b')' {
            i += 1;
            out.push(Tok::RParen);
        } else if c == b';' {
            i += 1;
            out.push(Tok::Semi);
        } else {
            i += 1;
            out.push(Tok::Other);
        }
    }
    out
}

fn import_spec(toks: &[Tok], i: &mut usize, imports: &mut Vec<Import>) {
    let name = match toks.get(*i) {
        Some(Tok::Ident(s)) => {
            *i += 1;
            Some(s.clone())
        }
        Some(Tok::Dot) => {
            *i += 1;
            Some(".".to_string())
        }
        _ => None,
    };
    if let Some(Tok::Str(p)) = toks.get(*i) {
        imports.push(Import { name, path: p.clone() });
        *i += 1;
    }
}

pub fn scan(src: &[u8]) -> FileUse {
    let toks = lex(src);
    let mut fu = FileUse::default();
    let mut i = 0;
    let skip_semis = |i: &mut usize| {
        while matches!(toks.get(*i), Some(Tok::Semi)) {
            *i += 1;
        }
    };
    skip_semis(&mut i);
    if matches!(toks.get(i), Some(Tok::Ident(s)) if s == "package") {
        i += 2;
    }
    loop {
        skip_semis(&mut i);
        if !matches!(toks.get(i), Some(Tok::Ident(s)) if s == "import") {
            break;
        }
        i += 1;
        if matches!(toks.get(i), Some(Tok::LParen)) {
            i += 1;
            loop {
                skip_semis(&mut i);
                if matches!(toks.get(i), None | Some(Tok::RParen)) {
                    i += 1;
                    break;
                }
                let before = i;
                import_spec(&toks, &mut i, &mut fu.imports);
                if i == before {
                    i += 1;
                }
            }
        } else {
            import_spec(&toks, &mut i, &mut fu.imports);
        }
    }
    let toks: Vec<&Tok> = toks.get(i..).unwrap_or_default().iter().filter(|t| !matches!(t, Tok::Semi)).collect();
    let n = toks.len();
    let mut j = 0;
    while j + 2 < n {
        if let (Tok::Ident(a), Tok::Dot, Tok::Ident(b)) = (toks[j], toks[j + 1], toks[j + 2]) {
            let prev_dot = j > 0 && matches!(toks[j - 1], Tok::Dot);
            if !prev_dot {
                fu.selectors.insert((a.clone(), b.clone()));
            }
            j += 2;
            continue;
        }
        j += 1;
    }
    fu
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Roots {
    pub names: BTreeMap<String, BTreeSet<String>>,
    pub all: BTreeSet<String>,
}

pub fn roots(files: &[FileUse], pkg_name: &dyn Fn(&str) -> Option<String>) -> Roots {
    let mut r = Roots::default();
    for f in files {
        for Import { name, path } in &f.imports {
            let local = match name.as_deref() {
                Some("_") => {
                    r.names.entry(path.clone()).or_default();
                    continue;
                }
                Some(".") => {
                    r.all.insert(path.clone());
                    continue;
                }
                Some(n) => n.to_string(),
                None => match pkg_name(path) {
                    Some(n) => n,
                    None => {
                        r.all.insert(path.clone());
                        continue;
                    }
                },
            };
            let set = r.names.entry(path.clone()).or_default();
            for (a, b) in &f.selectors {
                if *a == local {
                    set.insert(b.clone());
                }
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_imports_and_selectors() {
        let src = br#"package p

import (
	"fmt"
	x "ex/a"
	. "ex/dot"
	_ "ex/blank"
)
import "strings"

// a.Fake in a comment
func F() { s := "x.Y"; fmt.Println(x.T{}.M(), strings.ToUpper(s), 1.5, x.  Z) ; _ = r.(type) }
"#;
        let fu = scan(src);
        assert_eq!(fu.imports.len(), 5);
        assert!(fu.imports.iter().any(|i| i.name.as_deref() == Some(".") && i.path == "ex/dot"));
        assert!(fu.imports.iter().any(|i| i.name.is_none() && i.path == "strings"));
        assert!(fu.selectors.contains(&("x".into(), "T".into())));
        assert!(fu.selectors.contains(&("x".into(), "Z".into())));
        assert!(fu.selectors.contains(&("fmt".into(), "Println".into())));
        assert!(!fu.selectors.contains(&("x".into(), "Y".into())));
        assert!(!fu.selectors.contains(&("a".into(), "Fake".into())));
        assert!(!fu.selectors.contains(&("T".into(), "M".into())));
    }
}
