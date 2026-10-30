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
        // And what the build kept that no line table names: the headers.
        files.extend(self.kept_files(read));
        files.sort();
        files.dedup();
        for file in &files {
            let Some(text) = self.source_file(file, read) else { continue };
            for (name, kind, line, declared) in scan(file, &String::from_utf8_lossy(&text)) {
                symbols.entry((name.clone(), file.clone(), line)).or_insert((kind, name, declared));
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
/// line, declared)`, lines from 1, `declared` for a C prototype or
/// `extern`, which only says it is defined elsewhere.
pub fn scan(file: &str, text: &str) -> Vec<(String, &'static str, u32, bool)> {
    let rust = file.ends_with(".rs");
    let c = [".c", ".h"].iter().any(|e| file.ends_with(e));
    let mut out = Vec::new();
    let mut c_state = CState::default();
    for (n, line) in text.lines().enumerate() {
        let line_number = n as u32 + 1;
        if rust {
            if let Some((name, kind)) = rust_item(line.trim_start()) {
                out.push((name, kind, line_number, false));
            }
        } else if c {
            out.extend(c_state.line(line).into_iter().map(|(name, kind, declared)| (name, kind, line_number, declared)));
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

/// The last identifier in `s`, before whatever is not one.
fn last_ident(s: &str) -> Option<String> {
    let s = s.trim_end();
    let start = s.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |i| i + 1);
    ident(&s[start..])
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

/// Words a C line can begin that begin no definition of a name.
const C_STATEMENTS: [&str; 12] = ["if", "else", "for", "while", "do", "switch", "case", "return", "goto", "break", "continue", "default"];

/// Reading C a line at a time: what each line defines, knowing whether it
/// is inside an enum's braces, and a comment's.
#[derive(Default)]
struct CState {
    /// Inside `enum … {`, how deep in braces.
    enumeration: Option<usize>,
    /// Inside `/* … */`.
    comment: bool,
}

impl CState {
    fn line(&mut self, line: &str) -> Vec<(String, &'static str, bool)> {
        // Comments out, as far as one line can say.
        let mut code = String::new();
        let mut rest = line;
        loop {
            if self.comment {
                match rest.find("*/") {
                    Some(at) => {
                        rest = &rest[at + 2..];
                        self.comment = false;
                    }
                    None => break,
                }
            } else {
                let block = rest.find("/*");
                let slashes = rest.find("//");
                match (block, slashes) {
                    (Some(b), s) if s.is_none_or(|s| b < s) => {
                        code.push_str(&rest[..b]);
                        code.push(' ');
                        rest = &rest[b + 2..];
                        self.comment = true;
                    }
                    (_, Some(s)) => {
                        code.push_str(&rest[..s]);
                        break;
                    }
                    _ => {
                        code.push_str(rest);
                        break;
                    }
                }
            }
        }
        let code = code.trim_end();
        let t = code.trim_start();
        // An enum's constants, each at the start of its line.
        if let Some(depth) = self.enumeration {
            let opened = t.matches('{').count();
            let closed = t.matches('}').count();
            let depth = depth + opened;
            let mut out = Vec::new();
            if depth > 0
                && let Some(name) = ident(t)
                && t[name.len()..].trim_start().starts_with([',', '='])
                    | t[name.len()..].trim().is_empty()
            {
                out.push((name, "const", false));
            }
            self.enumeration = depth.checked_sub(closed).filter(|d| *d > 0 || closed == 0);
            if closed > 0 && self.enumeration.is_none() && let Some(name) = c_closing(t) {
                out.push((name, "type", false));
            }
            return out;
        }
        if let Some(after) = t.strip_prefix('#') {
            let after = after.trim_start();
            return after
                .strip_prefix("define")
                .filter(|a| a.starts_with([' ', '\t']))
                .and_then(|a| ident(a.trim_start()))
                .map(|n| vec![(n, "macro", false)])
                .unwrap_or_default();
        }
        if t.is_empty() || code.starts_with([' ', '\t']) {
            return Vec::new();
        }
        // At the start of a line, as a file's own are.
        if code.starts_with('}') {
            return c_closing(code).map(|n| vec![(n, "type", false)]).unwrap_or_default();
        }
        if ident(code).is_some_and(|w| C_STATEMENTS.contains(&w.as_str())) {
            return Vec::new();
        }
        let words: Vec<&str> = code.split_whitespace().collect();
        let enumeration = words.iter().any(|w| *w == "enum" || w.starts_with("enum{"));
        if enumeration && !code.ends_with(';') {
            self.enumeration = Some(code.matches('{').count());
            let named = words.iter().position(|w| *w == "enum").and_then(|i| words.get(i + 1)).and_then(|w| ident(w));
            return named.map(|n| vec![(n, "type", false)]).unwrap_or_default();
        }
        for keyword in ["struct ", "union "] {
            if let Some(after) = code.strip_prefix(keyword) {
                let Some(name) = ident(after.trim_start()) else { return Vec::new() };
                let tail = after.trim_start()[name.len()..].trim();
                return if tail.is_empty() || tail.starts_with('{') { vec![(name, "type", false)] } else { Vec::new() };
            }
        }
        if let Some(after) = code.strip_prefix("typedef ") {
            if after.contains('(') {
                // `typedef void (*name)(…);`
                return after.split_once("(*").and_then(|(_, n)| ident(n.trim_start())).map(|n| vec![(n, "type", false)]).unwrap_or_default();
            }
            return after.strip_suffix(';').and_then(last_ident).map(|n| vec![(n, "type", false)]).unwrap_or_default();
        }
        let extern_ = code.starts_with("extern ");
        // A function: its name before its parameters; a prototype ends
        // with `;`.
        if let Some(open) = code.find('(')
            && !code[..open].contains('=')
        {
            let Some(name) = last_ident(&code[..open]) else { return Vec::new() };
            // Its name alone, its type on the line before, is a
            // definition's only when the line ends as one's head does:
            // otherwise it is a call, or a macro used.
            let bare = code[..open].trim() == name;
            if C_STATEMENTS.contains(&name.as_str()) || (bare && !code.ends_with(')') && !code.ends_with('{')) {
                return Vec::new();
            }
            return vec![(name, "function", extern_ || code.ends_with(';'))];
        }
        // A variable: its name before its initializer, its size or `;`.
        let end = code.find(['=', '[', ';']).unwrap_or(code.len());
        if end == code.len() && !code.ends_with(',') {
            return Vec::new();
        }
        if code[..end].split_whitespace().count() < 2 {
            return Vec::new();
        }
        last_ident(&code[..end]).map(|n| vec![(n, "static", extern_)]).unwrap_or_default()
    }
}

/// A typedef's `} name;`.
fn c_closing(t: &str) -> Option<String> {
    let after = t.strip_prefix('}')?.trim();
    let body = after.strip_suffix(';')?;
    last_ident(body)
}

#[cfg(test)]
mod tests {
    use super::scan;

    #[test]
    fn what_a_rust_file_defines_line_by_line() {
        let text = "pub(crate) const unsafe fn copy() {}\n#[derive(Debug)]\npub struct Task {\n    pub const MAX: usize = 3;\nmacro_rules! ringbuf {\nstatic mut TICKS: u64 = 0;\n    let x = 1;\nimpl Task {\n    type Output = u8;\npub fn _x() {}\nconst _: () = ();";
        let found: Vec<_> = scan("kern/src/task.rs", text).into_iter().map(|(n, k, l, _)| format!("{n} {k} {l}")).collect();
        assert_eq!(found, ["copy function 1", "Task type 3", "MAX const 4", "ringbuf macro 5", "TICKS static 6", "Output type 9", "_x function 10"]);
    }

    #[test]
    fn what_a_c_file_defines_line_by_line() {
        let text = "#define FRACBITS 16\n# define SCREENWIDTH 320\ntypedef int fixed_t;\ntypedef struct\n{\n    int x;\n} vertex_t;\nstruct node_s {\n    struct line_s *l;\n};\ntypedef void (*actionf_v)();\n#include <stdio.h>\n/* void commented(void); */\nvoid R_DrawColumn (void);\nextern int centerx;\nfixed_t centery;\nbyte *translationtables = NULL;\nstatic int widths[4] = { 1, 2 };\nvoid\nR_DrawColumn (void)\n{\n    if (x) foo(1);\n}\ntypedef enum\n{\n    sk_baby,\n    sk_easy = 1,\n    sk_hard\n} skill_t;\nenum { pw_invulnerability, NUMPOWERS };\nI_Error (\"x\");";
        let found: Vec<_> = scan("doomgeneric/r_draw.c", text).into_iter().map(|(n, k, l, d)| format!("{n} {k} {l}{}", if d { " declared" } else { "" })).collect();
        assert_eq!(
            found,
            [
                "FRACBITS macro 1",
                "SCREENWIDTH macro 2",
                "fixed_t type 3",
                "vertex_t type 7",
                "node_s type 8",
                "actionf_v type 11",
                "R_DrawColumn function 14 declared",
                "centerx static 15 declared",
                "centery static 16",
                "translationtables static 17",
                "widths static 18",
                "R_DrawColumn function 20",
                "sk_baby const 26",
                "sk_easy const 27",
                "sk_hard const 28",
                "skill_t type 29",
            ]
        );
    }
}
