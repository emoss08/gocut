// Ignore patterns for `gocut watch`: a gitignore-style subset, read from `.gocutignore` in the
// watched root and from `--ignore`. A file that matches never starts a build or a restart, and a
// directory that matches is not watched.
//
// Syntax, one pattern per line:
//
// - Blank lines and lines starting with `#` are skipped. `\#` and `\!` start a literal `#` or `!`.
// - A pattern with no `/` but a trailing one matches a file or directory name at any depth
//   (`*_mock.go`, `mocks`). A pattern with a `/` elsewhere matches the root-relative path
//   (`internal/gen/*.go`, `/tools`); a leading `/` only anchors it.
// - A trailing `/` matches directories only (`fixtures/`).
// - `*` matches any run of characters except `/`, `?` one character except `/`, `[abc]`, `[a-z]`
//   and `[!abc]` one character from (or not from) a set, and `**` any run including `/`, so
//   `**/x` matches `x` at any depth and `x/**` everything under `x`.
// - `!` re-includes what an earlier pattern ignored; the last matching pattern wins. As with
//   gitignore, a file under an ignored directory cannot be re-included.

use std::path::{Component, Path};

use anyhow::{Context, Result};

pub(crate) const FILE: &str = ".gocutignore";

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Ignore {
    rules: Vec<Rule>,
}

#[derive(Debug, PartialEq)]
struct Rule {
    glob: String,
    negate: bool,
    dir_only: bool,
    // Matched against the whole root-relative path, rather than against each name.
    anchored: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
}

impl Ignore {
    // Reads `.gocutignore` in `root`; a missing file means no patterns.
    pub(crate) fn load(root: &Path) -> Result<Ignore> {
        let path = root.join(FILE);
        let mut ig = Ignore::default();
        match std::fs::read_to_string(&path) {
            Ok(text) => text.lines().for_each(|l| ig.add(l)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
        Ok(ig)
    }

    pub(crate) fn add(&mut self, line: &str) {
        let line = line.trim_end_matches('\r');
        // Trailing spaces are dropped unless escaped, as in gitignore.
        let line = match line.trim_end_matches(' ') {
            t if t.ends_with('\\') && t.len() < line.len() => &line[..=t.len()],
            t => t,
        };
        if line.is_empty() || line.starts_with('#') {
            return;
        }
        let (negate, line) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line.strip_prefix('\\').filter(|r| r.starts_with(['#', '!'])).unwrap_or(line)),
        };
        let (dir_only, line) = match line.strip_suffix('/') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let anchored = line.contains('/');
        let glob = line.strip_prefix('/').unwrap_or(line);
        if glob.is_empty() {
            return;
        }
        self.rules.push(Rule { glob: glob.to_string(), negate, dir_only, anchored });
    }

    pub(crate) fn len(&self) -> usize {
        self.rules.len()
    }

    // Whether `rel`, a path relative to the watched root, is ignored: it matches itself, or one of
    // its parent directories does.
    pub(crate) fn matches(&self, rel: &Path, kind: Kind) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let parts: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        (1..=parts.len()).any(|n| {
            let kind = if n == parts.len() { kind } else { Kind::Dir };
            self.decide(&parts[..n].join("/"), &parts[n - 1], kind)
        })
    }

    // The last rule that matches decides.
    fn decide(&self, path: &str, name: &str, kind: Kind) -> bool {
        self.rules
            .iter()
            .rev()
            .find(|r| {
                (!r.dir_only || kind == Kind::Dir)
                    && glob(r.glob.as_bytes(), if r.anchored { path } else { name }.as_bytes())
            })
            .is_some_and(|r| !r.negate)
    }
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p {
        [] => s.is_empty(),
        [b'*', b'*', rest @ ..] => {
            // `**/` also matches no directories at all.
            if let [b'/', after @ ..] = rest
                && glob(after, s)
            {
                return true;
            }
            (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        [b'*', rest @ ..] => {
            let run = s.iter().position(|&c| c == b'/').unwrap_or(s.len());
            (0..=run).any(|i| glob(rest, &s[i..]))
        }
        [b'?', rest @ ..] => matches!(s, [c, tail @ ..] if *c != b'/' && glob(rest, tail)),
        [b'[', rest @ ..] => match (class(rest), s) {
            (Some((set, after)), [c, tail @ ..]) => *c != b'/' && set(*c) && glob(after, tail),
            // An unclosed `[` is a literal.
            (None, [b'[', tail @ ..]) => glob(rest, tail),
            (Some(_), []) | (None, _) => false,
        },
        [b'\\', c, rest @ ..] | [c, rest @ ..] => matches!(s, [d, tail @ ..] if d == c && glob(rest, tail)),
    }
}

// Parses a character class after its `[`: a predicate for the bytes it accepts, and the pattern
// after its `]`. None when the class is not closed.
fn class(p: &[u8]) -> Option<(impl Fn(u8) -> bool + '_, &[u8])> {
    let (negate, body) = match p {
        [b'!' | b'^', rest @ ..] => (true, rest),
        _ => (false, p),
    };
    // A `]` right after the opening (or after `!`) is a member, not the end.
    let end = body.iter().skip(1).position(|&c| c == b']')? + 1;
    let set = &body[..end];
    let accept = move |c: u8| {
        let mut i = 0;
        let mut hit = false;
        while i < set.len() {
            if i + 2 < set.len() && set[i + 1] == b'-' {
                hit |= (set[i]..=set[i + 2]).contains(&c);
                i += 3;
            } else {
                hit |= set[i] == c;
                i += 1;
            }
        }
        hit != negate
    };
    Some((accept, &body[end + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignore(lines: &[&str]) -> Ignore {
        let mut ig = Ignore::default();
        for l in lines {
            ig.add(l);
        }
        ig
    }

    #[test]
    fn globs() {
        let g = |p: &str, s: &str| glob(p.as_bytes(), s.as_bytes());
        assert!(g("*_mock.go", "store_mock.go"));
        assert!(!g("*_mock.go", "store_mock.go.bak"));
        assert!(!g("*.go", "a/b.go"), "* stops at /");
        assert!(g("?.go", "a.go"));
        assert!(!g("?.go", "ab.go"));
        assert!(g("**/gen.go", "gen.go"));
        assert!(g("**/gen.go", "a/b/gen.go"));
        assert!(g("a/**", "a/b/c.go"));
        assert!(!g("a/**", "ab/c.go"));
        assert!(g("a/**/z.go", "a/z.go"));
        assert!(g("a/**/z.go", "a/b/c/z.go"));
        assert!(g("[abc].go", "b.go"));
        assert!(!g("[abc].go", "d.go"));
        assert!(g("[a-c]x", "bx"));
        assert!(g("[!a-c]x", "dx"));
        assert!(!g("[!a-c]x", "ax"));
        assert!(g("[]]", "]"));
        assert!(g("[x", "[x"), "an unclosed [ is literal");
        assert!(g(r"\*.go", "*.go"));
        assert!(!g(r"\*.go", "a.go"));
    }

    #[test]
    fn lines_parse_into_rules() {
        let ig = ignore(&["", "# comment", "  ", "*_mock.go", "!keep_mock.go", "/tools/", r"\#hash", "/", "trail  "]);
        let rule = |glob: &str, negate, dir_only, anchored| Rule { glob: glob.into(), negate, dir_only, anchored };
        assert_eq!(
            ig.rules,
            vec![
                rule("*_mock.go", false, false, false),
                rule("keep_mock.go", true, false, false),
                rule("tools", false, true, true),
                rule("#hash", false, false, false),
                rule("trail", false, false, false),
            ]
        );
    }

    #[test]
    fn unanchored_patterns_match_names_at_any_depth_and_anchored_ones_the_relative_path() {
        let ig = ignore(&["*_mock.go", "mocks", "internal/gen/*.go", "/tools"]);
        let file = |p: &str| ig.matches(Path::new(p), Kind::File);
        assert!(file("store_mock.go"));
        assert!(file("internal/store/store_mock.go"));
        assert!(file("mocks/store.go"));
        assert!(file("internal/mocks/store.go"));
        assert!(file("internal/gen/models.go"));
        assert!(!file("pkg/internal/gen/models.go"), "anchored at the root");
        assert!(!file("internal/gen/sub/models.go"), "* stops at /");
        assert!(file("tools/tools.go"));
        assert!(!file("cmd/tools/main.go"));
        assert!(!file("internal/store/store.go"));
        assert!(ig.matches(Path::new("internal/mocks"), Kind::Dir));
    }

    #[test]
    fn a_trailing_slash_matches_directories_only() {
        let ig = ignore(&["fixtures/"]);
        assert!(ig.matches(Path::new("pkg/fixtures"), Kind::Dir));
        assert!(ig.matches(Path::new("pkg/fixtures/a.go"), Kind::File));
        assert!(!ig.matches(Path::new("pkg/fixtures"), Kind::File));
    }

    #[test]
    fn the_last_matching_pattern_wins_but_an_ignored_directory_stays_ignored() {
        let ig = ignore(&["*_gen.go", "!wire_gen.go", "mocks/", "!mocks/keep.go"]);
        let file = |p: &str| ig.matches(Path::new(p), Kind::File);
        assert!(file("a/models_gen.go"));
        assert!(!file("a/wire_gen.go"));
        assert!(file("mocks/keep.go"), "a file under an ignored directory cannot be re-included");
        assert!(!ignore(&["!x.go", "x.go", "!x.go"]).matches(Path::new("x.go"), Kind::File));
    }

    #[test]
    fn no_patterns_ignore_nothing() {
        assert!(!Ignore::default().matches(Path::new("a/b.go"), Kind::File));
    }

    #[test]
    fn a_missing_file_loads_as_no_patterns() {
        let ig = Ignore::load(Path::new("/nonexistent-gocut")).unwrap();
        assert_eq!(ig.len(), 0);
    }
}
