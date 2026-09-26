//! Deterministic ESM -> CommonJS rewrite for ONE browser-entry source file
//! (browser-esm-rewrite).
//!
//! WHY. A browser artifact is ONE `async function (request, ops)` expression
//! ([`crate::browser_artifacts::bundle`]) with the deployment's entry file
//! embedded VERBATIM in its body, so a static `import`/`export` STATEMENT is a
//! hard SyntaxError there — a function body is not a module, in any substrate.
//! The substrate is now vendored node-worker (Node v25.9.0's own lib
//! transpiled for a Worker) with a real CommonJS AND ESM loader, but a loader
//! loads FILES while the artifact is an EXPRESSION. So rather than rejecting
//! ESM entries — which is what excluded every framework build output
//! (`dist/server/entry.mjs`, `.output/server/index.mjs`, a SvelteKit
//! `build/index.js`) from browser nodes — the build rewrites the module syntax
//! into the CommonJS the envelope and the guest's `require`/`module.exports`
//! registry (the Node API the guest runtime installs) already supply.
//!
//! WHAT IT REFUSES. Only what has no equivalent CommonJS: an entry mixing
//! `export default` with named exports (one artifact resolves ONE handler), an
//! unrecognized export form, and an unterminated module statement. A module
//! the SUBSTRATE cannot provide (a relative specifier, `net`, …) is named at
//! the point of use by the guest runtime's honesty rule — never turned into a
//! build-time guess here.
//!
//! DETERMINISM. A pure string transform over LF-normalized input: identical
//! input always yields identical output, so artifact digests stay stable.
//! An already-CommonJS entry comes back byte-identical, so artifacts built
//! before this existed keep their digests and are never rebuilt for nothing.
//!
//! NO CRATE DEPENDENCIES ON PURPOSE: this is a self-contained scanner, so it
//! can be exercised standalone against real framework output.

/// One entry file's rewrite outcome.
pub struct EsmRewrite {
    /// The CommonJS source to embed in the artifact envelope.
    pub source: String,
    /// True when the entry carried module syntax that was rewritten.
    pub rewritten: bool,
    /// True when the rewritten source references the `import.meta` shim, so
    /// the envelope must define it.
    pub uses_import_meta: bool,
}

/// Rewrite `src` (LF-normalized) into CommonJS. `Err` names the construct that
/// has no CommonJS equivalent — the caller turns it into a loud build
/// rejection (explicit opt-in) or a silent skip (auto-detected entry).
pub fn rewrite_esm(src: &str) -> Result<EsmRewrite, String> {
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0usize;
    let mut saw_module = false;
    let mut saw_default = false;
    let mut saw_named = false;
    let mut ns_seq = 0usize;
    while i < lines.len() {
        match module_keyword(lines[i].trim_start()) {
            None => {
                out.push(lines[i].to_string());
                i += 1;
            }
            Some(Kw::Import) => {
                let (stmt, used) = gather(&lines, i)?;
                i += used;
                saw_module = true;
                out.push(rewrite_import(&stmt)?);
            }
            Some(Kw::Export) => {
                let (stmt, used) = gather(&lines, i)?;
                i += used;
                saw_module = true;
                let (text, is_default, is_named) = rewrite_export(&stmt, &mut ns_seq)?;
                saw_default |= is_default;
                saw_named |= is_named;
                out.push(text);
            }
        }
    }
    if saw_default && saw_named {
        return Err(
            "the entry mixes `export default` with named exports — one browser artifact resolves \
             ONE handler, so export a default handler or a named `handler`, not both"
                .to_string(),
        );
    }
    if !saw_module {
        // Already CommonJS: returned unchanged, byte for byte.
        return Ok(EsmRewrite {
            source: src.to_string(),
            rewritten: false,
            uses_import_meta: false,
        });
    }
    let joined = out.join("\n");
    let source = if joined.contains("import.meta") {
        replace_import_meta(&joined)
    } else {
        joined
    };
    let uses_import_meta = source.contains(IMPORT_META_SHIM);
    Ok(EsmRewrite {
        source,
        rewritten: true,
        uses_import_meta,
    })
}

/// The identifier the envelope defines for `import.meta` — a single in-memory
/// file, so there is no real disk path to report.
pub const IMPORT_META_SHIM: &str = "__hive_import_meta";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kw {
    Import,
    Export,
}

/// Static module syntax only. Dynamic `import(` stays legal (it is a call, and
/// the envelope is async), and `import.meta` is rewritten, not gathered.
fn module_keyword(t: &str) -> Option<Kw> {
    if t.starts_with("import ")
        || t.starts_with("import{")
        || t.starts_with("import\"")
        || t.starts_with("import'")
        || t == "import"
    {
        Some(Kw::Import)
    } else if t.starts_with("export ") || t.starts_with("export{") || t == "export" {
        Some(Kw::Export)
    } else {
        None
    }
}

/// Lexer state: bracket depth, string/template state, and comments, so a
/// `{`/`;`/`from` inside a string, a template or a comment is never mistaken
/// for syntax.
#[derive(Default)]
struct State {
    depth: i32,
    quote: Option<char>,
    interp: Vec<i32>,
    line_comment: bool,
    block_comment: bool,
    escaped: bool,
}

impl State {
    fn feed(&mut self, c: char, next: Option<char>) {
        if self.line_comment {
            if c == '\n' {
                self.line_comment = false;
            }
            return;
        }
        if self.block_comment {
            if c == '*' && next == Some('/') {
                self.block_comment = false;
            }
            return;
        }
        if let Some(q) = self.quote {
            if self.escaped {
                self.escaped = false;
                return;
            }
            if c == '\\' {
                self.escaped = true;
                return;
            }
            if c == q {
                self.quote = None;
            } else if q == '`' && c == '$' && next == Some('{') {
                self.interp.push(self.depth);
                self.depth += 1;
            }
            return;
        }
        match c {
            '/' if next == Some('/') => self.line_comment = true,
            '/' if next == Some('*') => self.block_comment = true,
            '\'' | '"' | '`' => self.quote = Some(c),
            '(' | '[' | '{' => self.depth += 1,
            ')' | ']' | '}' => {
                if c == '}' {
                    if let Some(d) = self.interp.last().copied() {
                        if self.depth - 1 == d {
                            self.interp.pop();
                        }
                    }
                }
                self.depth -= 1;
            }
            _ => {}
        }
    }

    /// In code (not inside a string, template or comment) at the top level.
    fn top_level(&self) -> bool {
        self.depth <= 0 && self.in_code()
    }

    /// In code at any depth — a token rewrite (unlike a split) does not care
    /// how many brackets enclose it.
    fn in_code(&self) -> bool {
        self.quote.is_none() && !self.block_comment && !self.line_comment
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

struct Scan {
    /// Every string/template literal: (start, end-inclusive, raw with quotes).
    literals: Vec<(usize, usize, String)>,
    /// Every identifier at bracket depth 0: (start, word).
    words: Vec<(usize, String)>,
}

fn scan(s: &str) -> Scan {
    let chars: Vec<char> = s.chars().collect();
    let mut st = State::default();
    let mut out = Scan {
        literals: Vec::new(),
        words: Vec::new(),
    };
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let was_code = st.quote.is_none() && !st.line_comment && !st.block_comment;
        let depth_before = st.depth;
        if was_code && is_ident_start(c) {
            let start = i;
            let mut j = i;
            while j < chars.len() && is_ident(chars[j]) {
                j += 1;
            }
            if depth_before <= 0 {
                out.words.push((start, chars[start..j].iter().collect()));
            }
            // Re-feed the whole word so quote/comment state stays correct.
            for k in start..j {
                st.feed(chars[k], chars.get(k + 1).copied());
            }
            i = j;
            continue;
        }
        if was_code && (c == '\'' || c == '"' || c == '`') {
            let start = i;
            let q = c;
            st.feed(c, next);
            i += 1;
            while i < chars.len() {
                let d = chars[i];
                let dn = chars.get(i + 1).copied();
                if st.escaped {
                    st.escaped = false;
                    i += 1;
                    continue;
                }
                if d == '\\' {
                    st.escaped = true;
                    i += 1;
                    continue;
                }
                if d == q {
                    st.feed(d, dn);
                    i += 1;
                    break;
                }
                st.feed(d, dn);
                i += 1;
            }
            out.literals.push((start, i - 1, chars[start..i].iter().collect()));
            continue;
        }
        st.feed(c, next);
        i += 1;
    }
    out
}

/// Index of the standalone keyword `word` at bracket depth 0, outside strings
/// and comments.
fn find_word(s: &str, word: &str) -> Option<usize> {
    scan(s)
        .words
        .into_iter()
        .find(|(_, w)| w == word)
        .map(|(i, _)| i)
}

/// The first string literal at or after `from`: the module specifier.
fn specifier_at(s: &str, from: usize) -> Option<(usize, usize, String)> {
    scan(s)
        .literals
        .into_iter()
        .find(|(start, _, _)| *start > from)
}

fn first_ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !is_ident(c))
        .unwrap_or(s.len());
    if end == 0 {
        None
    } else {
        Some(s[..end].to_string())
    }
}

/// Rewrite `import.meta` to the envelope's shim, in CODE only: a mention
/// inside a string, template or comment is data, not syntax, and must be left
/// alone.
fn replace_import_meta(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let token: Vec<char> = "import.meta".chars().collect();
    let mut st = State::default();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        if st.in_code() && starts_with(&chars, i, &token) {
            let after = i + token.len();
            let prev_ok = i == 0 || !is_ident(chars[i - 1]);
            let next_ok = after >= chars.len() || !is_ident(chars[after]);
            if prev_ok && next_ok {
                out.push_str(IMPORT_META_SHIM);
                for k in i..after {
                    st.feed(chars[k], chars.get(k + 1).copied());
                }
                i = after;
                continue;
            }
        }
        st.feed(chars[i], chars.get(i + 1).copied());
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn starts_with(chars: &[char], at: usize, token: &[char]) -> bool {
    if at + token.len() > chars.len() {
        return false;
    }
    chars[at..at + token.len()] == *token
}

/// A statement is complete at a line end when it is not mid-expression: no
/// open bracket/string (checked separately) and no dangling operator.
fn statement_complete(acc: &str) -> bool {
    let s = acc.trim_end();
    if s.ends_with(';') {
        return true;
    }
    if s.ends_with("=>") || s.ends_with("...") {
        return false;
    }
    !matches!(
        s.chars().last(),
        Some('(' | '[' | '{' | ',' | ':' | '.' | '=' | '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^' | '?' | '<' | '>' | '!' | '~' | '\\')
    )
}

/// Gather one module statement starting at `lines[start]`, spanning as many
/// lines as it takes to close (imports and `export default` bodies are
/// routinely multi-line). Returns (statement, lines consumed).
fn gather(lines: &[&str], start: usize) -> Result<(String, usize), String> {
    let mut st = State::default();
    let mut acc = String::new();
    let mut i = start;
    let max_lines = 2000;
    while i < lines.len() && i - start < max_lines {
        let chars: Vec<char> = lines[i].chars().collect();
        let mut ended = false;
        for (j, c) in chars.iter().enumerate() {
            let next = chars.get(j + 1).copied();
            st.feed(*c, next);
            acc.push(*c);
            if *c == ';' && st.top_level() {
                ended = true;
                break;
            }
        }
        i += 1;
        if ended {
            return Ok((acc, i - start));
        }
        acc.push('\n');
        if st.top_level() && statement_complete(&acc) {
            return Ok((acc, i - start));
        }
    }
    Err(format!(
        "line {}: unterminated `{}` statement — the entry could not be read as one module",
        start + 1,
        lines[start].trim_start().split_whitespace().next().unwrap_or("module")
    ))
}

fn rewrite_import(stmt: &str) -> Result<String, String> {
    let s = stmt.trim();
    if let Some(from) = find_word(s, "from") {
        let spec = specifier_at(s, from);
        let Some((_, end, spec)) = spec else {
            return Err("an `import ... from ...` statement has no module specifier".to_string());
        };
        let clause = s[6..from].trim();
        // The head never carries the terminator; the statement's own tail
        // decides it, so exactly one `;` is emitted either way.
        let head = import_clause_decl(clause, &spec)?
            .trim_end()
            .trim_end_matches(';')
            .to_string();
        let tail = s[end + 1..].trim();
        if tail.is_empty() {
            return Ok(format!("{head};"));
        }
        if tail.ends_with(';') {
            return Ok(format!("{head}{tail}"));
        }
        return Ok(format!("{head}{tail};"));
    }
    let lit = scan(s).literals.into_iter().next();
    let Some((_, _, spec)) = lit else {
        return Err("an `import` statement has no module specifier".to_string());
    };
    Ok(format!("require({spec});"))
}

fn import_clause_decl(clause: &str, spec: &str) -> Result<String, String> {
    let c = clause.trim();
    if c.is_empty() {
        return Err("an `import ... from ...` statement has an empty binding".to_string());
    }
    if let Some(rest) = c.strip_prefix('*') {
        let ns = rest
            .trim_start()
            .strip_prefix("as")
            .unwrap_or("")
            .trim_start()
            .to_string();
        let ns = first_ident(&ns).unwrap_or_default();
        if ns.is_empty() {
            return Err("an `import * as ns` statement needs a namespace name".to_string());
        }
        return Ok(format!("const {ns} = require({spec});"));
    }
    if let Some(open) = c.find('{') {
        let close = c.rfind('}').filter(|x| *x > open).unwrap_or(c.len());
        let inner = &c[open + 1..close];
        let default = c[..open].trim().trim_end_matches(',').trim();
        let named = named_bindings(inner);
        if default.is_empty() {
            return Ok(format!("const {{ {named} }} = require({spec});"));
        }
        return Ok(format!(
            "const {{ default: {default}, {named} }} = require({spec});"
        ));
    }
    Ok(format!("const {c} = require({spec});"))
}

/// `a, b as c` -> `a, b: c` (an import clause has no nesting).
fn named_bindings(inner: &str) -> String {
    inner
        .split(',')
        .map(|part| {
            let p = part.trim();
            match p.split_once(" as ") {
                Some((l, r)) => format!("{}: {}", l.trim(), r.trim()),
                None => p.to_string(),
            }
        })
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rewrite one `export` statement. Returns (code, is_default, is_named).
fn rewrite_export(stmt: &str, ns_seq: &mut usize) -> Result<(String, bool, bool), String> {
    let s = stmt.trim();
    let rest = s.strip_prefix("export").unwrap_or(s).trim_start();
    let body = rest.trim_end().trim_end_matches(';').trim_end();
    if let Some(after) = default_export_body(body) {
        let a = after.trim_start();
        if let Some((code, assign)) = default_declaration(a)? {
            return Ok((format!("{code}\n{assign}"), true, false));
        }
        return Ok((format!("module.exports = {a};"), true, false));
    }
    for kw in ["const", "let", "var"] {
        if let Some(r) = word_prefix(body, kw) {
            let binding = r
                .split_once('=')
                .map(|(b, _)| b)
                .unwrap_or(r)
                .trim()
                .to_string();
            let names = declared_names(&binding);
            if names.is_empty() {
                return Err(format!(
                    "an `export {kw}` declaration whose binding could not be read — export the \
                     handler as `export const handler = ...` or `export default handler`"
                ));
            }
            let mut code = terminated(body);
            for n in &names {
                code.push_str(&format!("\nmodule.exports.{n} = {n};"));
            }
            return Ok((code, false, true));
        }
    }
    for kw in ["async function", "function", "class"] {
        if let Some(r) = word_prefix(body, kw) {
            let name = first_ident(r);
            let Some(name) = name else {
                return Err(format!(
                    "an `export {kw}` declaration without a name — a browser artifact resolves its \
                     handler from a named export"
                ));
            };
            return Ok((
                format!("{}\nmodule.exports.{name} = {name};", terminated(body)),
                false,
                true,
            ));
        }
    }
    if let Some(inner) = body.strip_prefix('{') {
        let close = inner.rfind('}').unwrap_or(inner.len());
        let inner = &inner[..close];
        let pairs = export_pairs(inner);
        if pairs.is_empty() {
            return Err("an `export {}` statement lists nothing to export".to_string());
        }
        if let Some(from) = find_word(body, "from") {
            let Some((_, _, spec)) = specifier_at(body, from) else {
                return Err("an `export ... from ...` statement has no module specifier".to_string());
            };
            let n = *ns_seq;
            *ns_seq += 1;
            let mut code = format!("const __hive_ns{n} = require({spec});");
            for (local, exported) in &pairs {
                code.push_str(&format!(
                    "\nmodule.exports.{exported} = __hive_ns{n}.{local};"
                ));
            }
            return Ok((code, false, true));
        }
        let mut code = String::new();
        for (local, exported) in &pairs {
            code.push_str(&format!("module.exports.{exported} = {local};\n"));
        }
        return Ok((code.trim_end().to_string(), false, true));
    }
    if let Some(r) = body.strip_prefix('*') {
        let r = r.trim_start();
        let (ns_name, from_part) = match r.strip_prefix("as") {
            Some(t) => {
                let name = first_ident(t).unwrap_or_default();
                (Some(name), t)
            }
            None => (None, r),
        };
        let Some(from) = find_word(from_part, "from") else {
            return Err("an `export *` statement needs `from \"<module>\"`".to_string());
        };
        let Some((_, _, spec)) = specifier_at(from_part, from) else {
            return Err("an `export *` statement has no module specifier".to_string());
        };
        let n = *ns_seq;
        *ns_seq += 1;
        let code = match ns_name {
            Some(name) if !name.is_empty() => {
                format!("const __hive_ns{n} = require({spec});\nmodule.exports.{name} = __hive_ns{n};")
            }
            _ => format!(
                "const __hive_ns{n} = require({spec});\n\
                 for (const __hive_k{n} in __hive_ns{n}) {{ \
                 if (__hive_k{n} !== \"default\" && __hive_k{n} !== \"__esModule\") \
                 module.exports[__hive_k{n}] = __hive_ns{n}[__hive_k{n}]; }}"
            ),
        };
        return Ok((code, false, true));
    }
    Err(format!(
        "unsupported `export` form ({:?}) — export the handler as `export default handler`, \
         `export const handler = ...`, or `export {{ handler }}`",
        truncate_for_message(body)
    ))
}

/// A declaration needs its terminator before the export assignment is appended
/// (a `const x = 1` without `;` would otherwise swallow it).
fn terminated(code: &str) -> String {
    let t = code.trim_end();
    if t.ends_with(';') || t.ends_with('}') {
        t.to_string()
    } else {
        format!("{t};")
    }
}

fn truncate_for_message(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 60 {
        format!("{}…", flat.chars().take(60).collect::<String>())
    } else {
        flat
    }
}

/// `body` after a `default` keyword (word-boundary checked), if it is one.
fn default_export_body(body: &str) -> Option<&str> {
    let after = body.strip_prefix("default")?;
    let next = after.chars().next();
    if after.is_empty()
        || next.is_none()
        || next.unwrap().is_whitespace()
        || matches!(next.unwrap(), '(' | '{' | '[' | '\'' | '"' | '`')
    {
        Some(after)
    } else {
        None
    }
}

/// A `export default` whose value is a DECLARATION is emitted verbatim and then
/// assigned; `None` means "treat the body as an expression".
fn default_declaration(a: &str) -> Result<Option<(String, String)>, String> {
    let decl = ["async function", "function", "class"];
    for kw in decl {
        let Some(r) = word_prefix(a, kw) else {
            continue;
        };
        let name = first_ident(r);
        let Some(name) = name else {
            // Anonymous: `export default function (req, res) {}` — assign the
            // expression directly; there is no name to reference afterwards.
            return Ok(Some((String::new(), format!("module.exports = {a};"))));
        };
        return Ok(Some((
            terminated(a),
            format!("module.exports = {name};"),
        )));
    }
    Ok(None)
}

/// `s` beginning with the keyword `kw` as a whole WORD -> the text after it.
fn word_prefix<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?;
    let next = rest.chars().next();
    if rest.is_empty() || next.unwrap().is_whitespace() || next.unwrap() == '{' {
        Some(rest)
    } else {
        None
    }
}

/// Names a declaration binds: `a`, `{a, b}`, `{a: c}`, `[a, b]`, `a = 1`.
fn declared_names(binding: &str) -> Vec<String> {
    let inner = binding.trim();
    let inner = if (inner.starts_with('{') && inner.ends_with('}'))
        || (inner.starts_with('[') && inner.ends_with(']'))
    {
        &inner[1..inner.len() - 1]
    } else {
        inner
    };
    split_top_level(inner, ',')
        .into_iter()
        .filter_map(|part| {
            let part = part.trim();
            let part = match split_top_level(part, ':').pop() {
                Some(after) if split_top_level(part, ':').len() > 1 => after,
                _ => part.to_string(),
            };
            let part = match split_top_level(&part, '=').first() {
                Some(before) if split_top_level(&part, '=').len() > 1 => before.clone(),
                _ => part.to_string(),
            };
            let part = part.trim().trim_matches('"').trim_matches('\'').trim();
            let name = first_ident(part)?;
            if name.chars().all(is_ident) && !name.is_empty() {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

/// `export { a, b as c }` -> [(local, exported)].
fn export_pairs(inner: &str) -> Vec<(String, String)> {
    split_top_level(inner, ',')
        .into_iter()
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let parts = split_top_level(part, ':');
            if parts.len() > 1 {
                return Some((parts[0].trim().to_string(), parts[1].trim().to_string()));
            }
            match part.split_once(" as ") {
                Some((l, r)) => Some((l.trim().to_string(), r.trim().to_string())),
                None => Some((part.to_string(), part.to_string())),
            }
        })
        .collect()
}

/// Split on `sep` at bracket depth 0, outside strings and comments.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut st = State::default();
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let next = chars.get(i + 1).copied();
        let at_top = st.top_level();
        if at_top && *c == sep {
            out.push(cur.clone());
            cur.clear();
            st.feed(*c, next);
            continue;
        }
        st.feed(*c, next);
        cur.push(*c);
    }
    out.push(cur);
    out
}
