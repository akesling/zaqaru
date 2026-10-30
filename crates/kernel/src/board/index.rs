//! An index of the board's code, to go from a name to where it was
//! defined: every function, static and type the archive's DWARF says
//! where it declared, for the kernel and each task, and what the DWARF
//! leaves out — Rust's types, traits, constants, modules and macros, which
//! rustc gives no place; C's macros; and functions inlined away whole —
//! found in the source the build kept, line by line.

use std::collections::BTreeMap;
use std::rc::Rc;

use armv6m::dwarf::Kind;
use armv6m::hubris::Image;

use super::Board;
use super::dwarf::ReadFile;
use crate::system::server::quoted;

impl Board {
    /// `board/index`: `{"files":[path…],"symbols":[[name, kind, file,
    /// line, path, declared]…]}`, each symbol's file an index into
    /// `files`, its path with the modules it is in where the DWARF says
    /// (otherwise its name), and `declared` 1 for one only declared there.
    /// Made once an archive.
    pub fn index_json(&self, map: Option<&Result<Image, String>>, read: ReadFile) -> Option<Rc<String>> {
        let image = map?.as_ref().ok()?;
        {
            let cache = self.debug.borrow();
            if self.archive.as_deref() == Some(cache.archive.as_str())
                && let Some(index) = cache.index.clone()
            {
                return Some(index);
            }
        }
        // By name, file and line: what is there, with the most known of it.
        let mut symbols: BTreeMap<(String, String, u32), (&'static str, String, bool)> = BTreeMap::new();
        let mut files: Vec<String> = Vec::new();
        for owner in std::iter::once(&image.kernel).chain(&image.tasks) {
            let Some(debug) = self.debug_of(&owner.name, read) else { continue };
            let Ok(debug) = &*debug else { continue };
            for d in debug.definitions() {
                let kind = match d.kind {
                    Kind::Function => "function",
                    Kind::Static => "static",
                    Kind::Type => "type",
                };
                let entry = symbols.entry((d.name, d.file, d.line)).or_insert((kind, d.path.clone(), d.declaration));
                // Defined, rather than declared, wins.
                if entry.2 && !d.declaration {
                    *entry = (kind, d.path, false);
                }
            }
            files.extend(debug.files());
        }
        files.sort();
        files.dedup();
        for file in &files {
            let Some(text) = self.source_file(file, read) else { continue };
            for (name, kind, line) in scan(file, &String::from_utf8_lossy(&text)) {
                symbols.entry((name.clone(), file.clone(), line)).or_insert((kind, name, false));
            }
        }
        let mut numbered: BTreeMap<&str, usize> = BTreeMap::new();
        let mut named: Vec<&str> = Vec::new();
        let rows: Vec<String> = symbols
            .iter()
            .map(|((name, file, line), (kind, path, declared))| {
                let n = *numbered.entry(file.as_str()).or_insert_with(|| {
                    named.push(file.as_str());
                    named.len() - 1
                });
                format!("[{},\"{kind}\",{n},{line},{},{}]", quoted(name), if path == name { "null".into() } else { quoted(path) }, u8::from(*declared))
            })
            .collect();
        let json = Rc::new(format!(
            r#"{{"files":[{}],"symbols":[{}]}}"#,
            named.iter().map(|f| quoted(f)).collect::<Vec<_>>().join(","),
            rows.join(",")
        ));
        self.debug.borrow_mut().index = Some(Rc::clone(&json));
        Some(json)
    }
}

/// What a source file defines that a line says plainly: `(name, kind,
/// line)`, lines from 1.
pub fn scan(file: &str, text: &str) -> Vec<(String, &'static str, u32)> {
    let rust = file.ends_with(".rs");
    let c = [".c", ".h"].iter().any(|e| file.ends_with(e));
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let found = if rust {
            rust_item(line.trim_start())
        } else if c {
            c_item(line)
        } else {
            None
        };
        if let Some((name, kind)) = found {
            out.push((name, kind, n as u32 + 1));
        }
    }
    out
}

/// An identifier at the start of `s`.
fn ident(s: &str) -> Option<String> {
    let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(s.len());
    let name = &s[..end];
    (!name.is_empty() && name != "_" && !name.starts_with(|c: char| c.is_ascii_digit())).then(|| name.to_string())
}

/// A Rust item a line begins: `pub(crate) const unsafe fn name`,
/// `struct Name`, `macro_rules! name`.
fn rust_item(line: &str) -> Option<(String, &'static str)> {
    let mut rest = line;
    if let Some(after) = rest.strip_prefix("pub") {
        rest = match after.strip_prefix('(') {
            Some(inside) => &inside[inside.find(')')? + 1..],
            None if after.starts_with(' ') => after,
            None => return None,
        }
        .trim_start();
    }
    loop {
        let before = rest;
        for qualifier in ["unsafe ", "async ", "default ", "extern \"C\" ", "extern \"Rust\" "] {
            rest = rest.strip_prefix(qualifier).unwrap_or(rest);
        }
        // `const fn`, as against a `const`.
        if rest.starts_with("const fn ") || rest.starts_with("const unsafe fn ") {
            rest = &rest[6..];
        }
        if rest == before {
            break;
        }
    }
    const ITEMS: [(&str, &str); 10] = [
        ("fn ", "function"),
        ("struct ", "type"),
        ("enum ", "type"),
        ("union ", "type"),
        ("type ", "type"),
        ("trait ", "trait"),
        ("const ", "const"),
        ("static ", "static"),
        ("mod ", "module"),
        ("macro_rules! ", "macro"),
    ];
    let (keyword, kind) = ITEMS.iter().find(|(keyword, _)| rest.starts_with(keyword))?;
    let after = rest[keyword.len()..].trim_start();
    let after = after.strip_prefix("mut ").unwrap_or(after);
    Some((ident(after)?, kind))
}

/// A C definition a line begins: `#define NAME`, `struct name {`, a
/// typedef's `} name;`, `typedef … name;` on one line.
fn c_item(line: &str) -> Option<(String, &'static str)> {
    let t = line.trim_start();
    if let Some(after) = t.strip_prefix('#') {
        let after = after.trim_start().strip_prefix("define")?;
        return after.starts_with([' ', '\t']).then(|| ident(after.trim_start())).flatten().map(|n| (n, "macro"));
    }
    // At the start of a line, as a file's own are.
    if line.starts_with(['}', 's', 'e', 'u', 't']) {
        if let Some(after) = line.strip_prefix('}') {
            let name = ident(after.trim_start().trim_end_matches(';').trim())?;
            return after.trim_end().ends_with(';').then_some((name, "type"));
        }
        for keyword in ["struct ", "enum ", "union "] {
            if let Some(after) = line.strip_prefix(keyword) {
                let name = ident(after.trim_start())?;
                let tail = after.trim_start()[name.len()..].trim();
                return (tail.is_empty() || tail.starts_with('{')).then_some((name, "type"));
            }
        }
        if let Some(after) = line.strip_prefix("typedef ")
            && !after.contains('(')
            && let Some(body) = after.trim_end().strip_suffix(';')
        {
            let start = body.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |i| i + 1);
            return ident(&body[start..]).map(|n| (n, "type"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::scan;

    #[test]
    fn what_a_rust_file_defines_line_by_line() {
        let text = "pub(crate) const unsafe fn copy() {}\n#[derive(Debug)]\npub struct Task {\n    pub const MAX: usize = 3;\nmacro_rules! ringbuf {\nstatic mut TICKS: u64 = 0;\n    let x = 1;\nimpl Task {\n    type Output = u8;\npub fn _x() {}\nconst _: () = ();";
        let found: Vec<_> = scan("kern/src/task.rs", text).into_iter().map(|(n, k, l)| format!("{n} {k} {l}")).collect();
        assert_eq!(found, ["copy function 1", "Task type 3", "MAX const 4", "ringbuf macro 5", "TICKS static 6", "Output type 9", "_x function 10"]);
    }

    #[test]
    fn what_a_c_file_defines_line_by_line() {
        let text = "#define FRACBITS 16\n# define SCREENWIDTH 320\ntypedef int fixed_t;\ntypedef struct\n{\n    int x;\n} vertex_t;\nstruct node_s {\nenum { A, B };\n    struct line_s *l;\ntypedef void (*actionf_v)();\n#include <stdio.h>";
        let found: Vec<_> = scan("doomgeneric/r_defs.h", text).into_iter().map(|(n, k, l)| format!("{n} {k} {l}")).collect();
        assert_eq!(found, ["FRACBITS macro 1", "SCREENWIDTH macro 2", "fixed_t type 3", "vertex_t type 7", "node_s type 8"]);
    }
}
