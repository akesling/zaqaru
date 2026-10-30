// Source as the page shows it, coloured: Rust and C, the languages the
// board's code is written in, by a scanner of their tokens — comments,
// strings, numbers, keywords, types, the functions called and the macros —
// over the whole file, so that a comment or a string over many lines is
// coloured on each of them.

const RUST_KEYWORDS = new Set(
  "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type union unsafe use where while".split(" "),
);
const RUST_TYPES = new Set("bool char str u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64".split(" "));
const C_KEYWORDS = new Set(
  "auto break case const continue default do else enum extern for goto if inline register restrict return sizeof static struct switch typedef union volatile while true false NULL".split(" "),
);
const C_TYPES = new Set("void char short int long float double signed unsigned bool boolean byte size_t ssize_t".split(" "));

// Each language's tokens, tried in order at each place: `[class, pattern]`,
// the class null for what is left as it is.
const COMMON = [
  ["c", /\/\/[^\n]*/y],
  ["c", /\/\*[\s\S]*?(?:\*\/|$)/y],
  ["n", /\b(?:0[xX][\da-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?)(?:[iu](?:8|16|32|64|128|size)|f32|f64|[uUlLfF]{1,3})?\b/y],
];
const RUST = [
  ...COMMON,
  ["a", /#!?\[[^\]\n]*\]/y],
  ["s", /b?r(#*)"[\s\S]*?"\1/y],
  ["s", /b?"(?:\\[\s\S]|[^"\\])*"/y],
  ["s", /b?'(?:\\(?:x[\da-fA-F]{2}|u\{[\da-fA-F]+\}|.)|[^\\'\n])'/y],
  ["l", /'[A-Za-z_]\w*/y],
  ["m", /[A-Za-z_]\w*!(?!=)/y],
  ["w", /[A-Za-z_]\w*/y],
];
const C = [
  ...COMMON,
  ["a", /#[ \t]*include[ \t]*<[^>\n]*>/y],
  ["a", /#[ \t]*[A-Za-z_]\w*/y],
  ["s", /[LuU]?"(?:\\[\s\S]|[^"\\\n])*"/y],
  ["s", /[LuU]?'(?:\\.|[^\\'\n])+'/y],
  ["w", /[A-Za-z_]\w*/y],
];

/// The language a file is in, by its name: `rust`, `c`, or null.
export function languageOf(file) {
  if (/\.rs$/.test(file)) return "rust";
  if (/\.(c|h|cc|cpp|hpp)$/.test(file)) return "c";
  return null;
}

/// `text`, the file `file`, as tokens: `[class, text]`, the class null for
/// plain text — `c` a comment, `s` a string or character, `n` a number,
/// `k` a keyword, `t` a type, `f` a function called or defined, `m` a
/// macro, `a` an attribute or a preprocessor line, `l` a lifetime, `x` a
/// constant named in capitals.
export function tokens(text, file) {
  const language = languageOf(file);
  if (!language) return [[null, text]];
  const rules = language === "rust" ? RUST : C;
  const keywords = language === "rust" ? RUST_KEYWORDS : C_KEYWORDS;
  const types = language === "rust" ? RUST_TYPES : C_TYPES;
  const out = [];
  let plain = "";
  const push = (cls, t) => {
    if (!cls) return void (plain += t);
    if (plain) out.push([null, plain]);
    plain = "";
    out.push([cls, t]);
  };
  let i = 0;
  while (i < text.length) {
    // Not where a token can begin: plain, fast.
    const ch = text[i];
    if (!/[\w"'/#]/.test(ch)) {
      push(null, ch);
      i++;
      continue;
    }
    let matched = false;
    for (const [cls, pattern] of rules) {
      pattern.lastIndex = i;
      const m = pattern.exec(text);
      if (!m || !m[0]) continue;
      // A word in the middle of one — `x1`'s `1` — is not a number.
      if (cls === "n" && i > 0 && /\w/.test(text[i - 1])) continue;
      let kind = cls;
      if (cls === "w") kind = word(m[0], text, i, language, keywords, types);
      push(kind, m[0]);
      i += m[0].length;
      matched = true;
      break;
    }
    if (!matched) {
      push(null, ch);
      i++;
    }
  }
  if (plain) out.push([null, plain]);
  return out;
}

function word(w, text, at, language, keywords, types) {
  const after = at + w.length;
  if (keywords.has(w)) return "k";
  // Defined: Rust's `fn name`, generic or not.
  if (language === "rust" && /\bfn\s+$/.test(text.slice(Math.max(0, at - 8), at))) return "f";
  if (types.has(w) || (language === "c" && /_t$/.test(w))) return "t";
  // Called, or defined: followed by `(`, or in Rust by `::<…>(`.
  const rest = text.slice(after, after + 2);
  if (rest[0] === "(" || (rest[0] === " " && text[after + 1] === "(" && language === "c") || (language === "rust" && rest === "::" && text[after + 2] === "<")) return "f";
  if (language === "rust" && /^[A-Z]/.test(w) && /[a-z]/.test(w)) return "t";
  if (/^[A-Z][A-Z\d_]+$/.test(w) && w.length > 1) return "x";
  return null;
}

function escape(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/// `text` as HTML, a line each, its tokens in spans of class `hl-<class>`.
export function highlight(text, file) {
  const lines = [""];
  for (const [cls, t] of tokens(text, file)) {
    const parts = t.split("\n");
    parts.forEach((part, n) => {
      if (n > 0) lines.push("");
      if (part) lines[lines.length - 1] += cls ? `<span class="hl-${cls}">${escape(part)}</span>` : escape(part);
    });
  }
  return lines;
}
