//! Native implementation of PostgreSQL's `fuzzystrmatch` extension: fuzzy
//! string matching via edit distance (`levenshtein`,
//! `levenshtein_less_equal`) and phonetic codes (`soundex` / `difference`,
//! `metaphone`, and the Double Metaphone pair `dmetaphone` /
//! `dmetaphone_alt`).
//!
//! Every algorithm is a mechanical port of the PostgreSQL C sources
//! (`contrib/fuzzystrmatch/fuzzystrmatch.c`,
//! `contrib/fuzzystrmatch/dmetaphone.c`,
//! `src/backend/utils/adt/levenshtein.c`), so outputs match the server
//! exactly — including the less-documented corners: the banded
//! `levenshtein_less_equal` early-exit values, soundex's all-zero code for
//! letterless input (`difference('', '') = 4`), metaphone's argument
//! validation order, and Double Metaphone's byte-oriented handling of
//! non-ASCII input. The ports were validated against live PostgreSQL 16
//! output over a randomized ~12k-case corpus (words, punctuation, multibyte
//! text, negative/zero costs, and length-limit boundaries).
//!
//! All functions are STRICT: any SQL NULL argument yields NULL.

use super::{ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_i64, arg_text, no_such};
use crate::relational::SqlValue;
use crate::sql::error::{Result, SqlError};

/// The `fuzzystrmatch` registry entry (PostgreSQL ships version 1.2).
pub static DEF: ExtensionDef = ExtensionDef {
    name: "fuzzystrmatch",
    default_version: "1.2",
    comment: "determine similarities and distance between strings",
    requires: &[],
    functions: &[
        "levenshtein",
        "levenshtein_less_equal",
        "soundex",
        "difference",
        "metaphone",
        "dmetaphone",
        "dmetaphone_alt",
    ],
    types: &[],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

/// Scalar-function entry point. Every function is STRICT, so a NULL anywhere
/// short-circuits to NULL before any per-function validation (like PG).
fn call(_ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    match name {
        // levenshtein(source, target) / levenshtein(source, target, ins, del, sub)
        "levenshtein" => {
            let s = arg_text(args, 0, name)?;
            let t = arg_text(args, 1, name)?;
            let (ins_c, del_c, sub_c) = match args.len() {
                2 => (1, 1, 1),
                5 => (
                    arg_i64(args, 2, name)?,
                    arg_i64(args, 3, name)?,
                    arg_i64(args, 4, name)?,
                ),
                n => return Err(bad_arity(name, n, "2 or 5")),
            };
            Ok(int4(levenshtein_internal(
                &s, &t, ins_c, del_c, sub_c, None,
            )?))
        }
        // levenshtein_less_equal(source, target, max) /
        // levenshtein_less_equal(source, target, ins, del, sub, max)
        "levenshtein_less_equal" => {
            let s = arg_text(args, 0, name)?;
            let t = arg_text(args, 1, name)?;
            let (ins_c, del_c, sub_c, max_d) = match args.len() {
                3 => (1, 1, 1, arg_i64(args, 2, name)?),
                6 => (
                    arg_i64(args, 2, name)?,
                    arg_i64(args, 3, name)?,
                    arg_i64(args, 4, name)?,
                    arg_i64(args, 5, name)?,
                ),
                n => return Err(bad_arity(name, n, "3 or 6")),
            };
            Ok(int4(levenshtein_internal(
                &s,
                &t,
                ins_c,
                del_c,
                sub_c,
                Some(max_d),
            )?))
        }
        "soundex" => {
            check_arity(name, args, 1)?;
            Ok(SqlValue::Text(soundex(&arg_text(args, 0, name)?)))
        }
        "difference" => {
            check_arity(name, args, 2)?;
            let a = arg_text(args, 0, name)?;
            let b = arg_text(args, 1, name)?;
            Ok(int4(difference(&a, &b)))
        }
        "metaphone" => {
            check_arity(name, args, 2)?;
            let s = arg_text(args, 0, name)?;
            let max_output = arg_i64(args, 1, name)?;
            Ok(SqlValue::Text(metaphone(&s, max_output)?))
        }
        "dmetaphone" => {
            check_arity(name, args, 1)?;
            Ok(SqlValue::Text(
                double_metaphone(&arg_text(args, 0, name)?).0,
            ))
        }
        "dmetaphone_alt" => {
            check_arity(name, args, 1)?;
            Ok(SqlValue::Text(
                double_metaphone(&arg_text(args, 0, name)?).1,
            ))
        }
        _ => Err(no_such(name)),
    }
}

/// Wrong argument count for a known function name: PostgreSQL reports this
/// as an unknown function signature (SQLSTATE 42883).
fn bad_arity(name: &str, got: usize, want: &str) -> SqlError {
    SqlError::UndefinedFunction(format!("{name} takes {want} arguments; {got} given"))
}

fn check_arity(name: &str, args: &[SqlValue], want: usize) -> Result<()> {
    if args.len() == want {
        Ok(())
    } else {
        Err(bad_arity(name, args.len(), &want.to_string()))
    }
}

/// Render a computed distance as `int4`. PostgreSQL silently overflows its C
/// `int` on absurd cost inputs; saturating is the defined-behaviour analogue.
fn int4(v: i64) -> SqlValue {
    SqlValue::Int4(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

// ---------------------------------------------------------------------------
// Levenshtein distance (port of PostgreSQL src/backend/utils/adt/levenshtein.c)
// ---------------------------------------------------------------------------

/// PostgreSQL's `MAX_LEVENSHTEIN_STRLEN`: inputs are limited to this many
/// *characters* (the limit is checked after the empty-string shortcuts, so an
/// empty argument admits an arbitrarily long counterpart, exactly like PG).
const MAX_LEVENSHTEIN_STRLEN: usize = 255;

/// Compute the (possibly banded) Levenshtein distance between `source` and
/// `target`, character-based, with configurable insertion / deletion /
/// substitution costs.
///
/// `max_d = None` computes the exact distance (plain `levenshtein`, and
/// `levenshtein_less_equal` with a negative bound, which PostgreSQL treats as
/// unbounded). `max_d = Some(d)` (`d >= 0`) is the `levenshtein_less_equal`
/// band optimization: the result is exact when it is `<= d` and is otherwise
/// only guaranteed to be some value `> d` (this port reproduces PostgreSQL's
/// banded computation cell for cell, so even the ">= d" values match).
fn levenshtein_internal(
    source: &str,
    target: &str,
    ins_c: i64,
    del_c: i64,
    sub_c: i64,
    max_d: Option<i64>,
) -> Result<i64> {
    let s: Vec<char> = source.chars().collect();
    let t: Vec<char> = target.chars().collect();
    let (mc, nc) = (s.len(), t.len());

    // We can transform an empty s into t with n insertions, or a non-empty t
    // into an empty s with m deletions. (Checked before the length limit,
    // like PostgreSQL.)
    if mc == 0 {
        return Ok(nc as i64 * ins_c);
    }
    if nc == 0 {
        return Ok(mc as i64 * del_c);
    }

    if mc > MAX_LEVENSHTEIN_STRLEN || nc > MAX_LEVENSHTEIN_STRLEN {
        return Err(SqlError::InvalidParameter(
            "argument exceeds the maximum length of 255 characters".into(),
        ));
    }

    // Cell counts: the notional matrix is (mc + 1) x (nc + 1).
    let m1 = mc + 1;
    let n1 = nc + 1;

    // Band state. `max_d = Some(d)` keeps the band bookkeeping live; it is
    // dropped (like PG setting `max_d = -1`) when the bound cannot prune.
    let mut sub_c = sub_c;
    let mut start_column = 0usize;
    let mut stop_column = m1;
    let mut max_d = max_d.filter(|d| *d >= 0);
    if let Some(d) = max_d {
        let net_inserts = nc as i64 - mc as i64;
        let min_theo_d = if net_inserts < 0 {
            -net_inserts * del_c
        } else {
            net_inserts * ins_c
        };
        if min_theo_d > d {
            return Ok(d + 1);
        }
        // A substitution can always be replaced by insert + delete, so this
        // clamp never changes the result; it tightens the theoretical bounds.
        if ins_c + del_c < sub_c {
            sub_c = ins_c + del_c;
        }
        let max_theo_d = min_theo_d + sub_c * mc.min(nc) as i64;
        if d >= max_theo_d {
            max_d = None;
        } else if ins_c + del_c > 0 {
            // Initial stop column: each move right of the best column costs
            // at least one extra insert + delete pair.
            let slack_d = d - min_theo_d;
            let best_column = if net_inserts < 0 {
                (-net_inserts) as usize
            } else {
                0
            };
            stop_column = best_column + (slack_d / (ins_c + del_c)) as usize + 1;
            if stop_column > mc {
                stop_column = m1;
            }
        }
    }

    // Previous and current rows of the notional array. Cells outside the band
    // are never read before being written in PostgreSQL; initializing them to
    // a value > max_d keeps the port deterministic without changing results.
    let fill = max_d.map_or(0, |d| d + 1);
    let mut prev = vec![fill; m1];
    let mut curr = vec![fill; m1];
    #[allow(clippy::needless_range_loop)] // mirrors the C loop shape
    for i in start_column..stop_column {
        prev[i] = i as i64 * del_c;
    }

    for j in 1..n1 {
        let y = t[j - 1];

        // In the best case, values percolate down the diagonal unchanged, so
        // the stop column advances by one row unless already at the edge.
        if stop_column < m1 {
            prev[stop_column] = max_d.map_or(0, |d| d + 1);
            stop_column += 1;
        }

        let mut i = if start_column == 0 {
            curr[0] = j as i64 * ins_c;
            1
        } else {
            start_column
        };

        while i < stop_column {
            let ins = prev[i] + ins_c;
            let del = curr[i - 1] + del_c;
            let sub = prev[i - 1] + if s[i - 1] == y { 0 } else { sub_c };
            curr[i] = ins.min(del).min(sub);
            i += 1;
        }

        std::mem::swap(&mut prev, &mut curr);

        if let Some(d) = max_d {
            // The "zero point" is the column where the untransformed string
            // remainders have equal length; residual cost grows linearly with
            // the distance from it.
            let zp = j as i64 - (nc as i64 - mc as i64);
            let residual = |col: usize| -> i64 {
                let net_inserts = col as i64 - zp;
                if net_inserts > 0 {
                    net_inserts * ins_c
                } else {
                    -net_inserts * del_c
                }
            };
            while stop_column > 0 {
                let ii = stop_column - 1;
                if prev[ii] + residual(ii) <= d {
                    break;
                }
                stop_column -= 1;
            }
            while start_column < stop_column {
                if prev[start_column] + residual(start_column) <= d {
                    break;
                }
                prev[start_column] = d + 1;
                curr[start_column] = d + 1;
                start_column += 1;
            }
            if start_column >= stop_column {
                return Ok(d + 1);
            }
        }
    }

    // The final value was swapped from the current row into `prev`.
    Ok(prev[m1 - 1])
}

// ---------------------------------------------------------------------------
// Soundex (port of _soundex in contrib/fuzzystrmatch/fuzzystrmatch.c)
// ---------------------------------------------------------------------------

/// Digit codes for `A`..`Z` (`0` marks the "ignored" letters AEHIOUWY).
const SOUNDEX_TABLE: &[u8; 26] = b"01230120022455012623010202";

/// PostgreSQL's `soundex_code`: the table digit for ASCII letters, the
/// uppercased byte itself otherwise (relevant only to the adjacent-code
/// comparison across non-letter bytes).
fn soundex_code(b: u8) -> u8 {
    let up = b.to_ascii_uppercase();
    if up.is_ascii_uppercase() {
        SOUNDEX_TABLE[(up - b'A') as usize]
    } else {
        up
    }
}

/// The 4-byte Russell soundex code, or `[0; 4]` when the input contains no
/// ASCII letter (PostgreSQL returns an all-NUL buffer there, which renders as
/// the empty string and counts as 4 position matches in `difference`).
fn soundex4(input: &str) -> [u8; 4] {
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == bytes.len() {
        return [0; 4];
    }

    let mut out = [b'0'; 4];
    out[0] = bytes[i].to_ascii_uppercase();
    i += 1;
    let mut count = 1;
    while i < bytes.len() && count < 4 {
        // A letter is coded when its digit differs from the previous *byte*'s
        // code (vowels act as separators; duplicates collapse), like PG.
        if bytes[i].is_ascii_alphabetic() && soundex_code(bytes[i]) != soundex_code(bytes[i - 1]) {
            let code = soundex_code(bytes[i]);
            if code != b'0' {
                out[count] = code;
                count += 1;
            }
        }
        i += 1;
    }
    out
}

/// `soundex(text)`: 4-character code, or `''` for letterless input.
fn soundex(input: &str) -> String {
    let code = soundex4(input);
    if code[0] == 0 {
        String::new()
    } else {
        String::from_utf8_lossy(&code).into_owned()
    }
}

/// `difference(text, text)`: how many of the 4 soundex positions match.
fn difference(a: &str, b: &str) -> i64 {
    let (ca, cb) = (soundex4(a), soundex4(b));
    (0..4).filter(|&i| ca[i] == cb[i]).count() as i64
}

// ---------------------------------------------------------------------------
// Metaphone (port of _metaphone in contrib/fuzzystrmatch/fuzzystrmatch.c,
// itself from CPAN Text-Metaphone-1.96 by Michael G Schwern; the algorithm is
// Lawrence Philips', "Computer Language" December 1990)
// ---------------------------------------------------------------------------

/// PostgreSQL's `MAX_METAPHONE_STRLEN` (a byte limit, unlike levenshtein's).
const MAX_METAPHONE_STRLEN: usize = 255;

/// The 'sh' phoneme.
const SH: u8 = b'X';
/// The 'th' phoneme.
const TH: u8 = b'0';

/// Letter-property bit codes for `A`..`Z` (from the metaphone code table).
const METAPHONE_CODES: [u8; 26] = [
    1, 16, 4, 16, 9, 2, 4, 16, 9, 2, 0, 2, 2, 2, 1, 4, 0, 2, 4, 4, 1, 0, 0, 0, 8, 0,
];

fn metaphone_getcode(b: u8) -> u8 {
    let up = b.to_ascii_uppercase();
    if up.is_ascii_uppercase() {
        METAPHONE_CODES[(up - b'A') as usize]
    } else {
        0
    }
}

/// AEIOU.
fn is_mvowel(b: u8) -> bool {
    metaphone_getcode(b) & 1 != 0
}

/// CGPST: letters that form diphthongs when preceding H.
fn affects_h(b: u8) -> bool {
    metaphone_getcode(b) & 4 != 0
}

/// EIY: letters that make C and G soft.
fn makes_soft(b: u8) -> bool {
    metaphone_getcode(b) & 8 != 0
}

/// BDH: letters that prevent GH from becoming F.
fn no_gh_to_f(b: u8) -> bool {
    metaphone_getcode(b) & 16 != 0
}

/// `metaphone(text, int)` with PostgreSQL's argument validation. The empty
/// string short-circuits *before* the length validation, exactly like PG.
fn metaphone(input: &str, max_output: i64) -> Result<String> {
    if input.is_empty() {
        return Ok(String::new());
    }
    if input.len() > MAX_METAPHONE_STRLEN {
        return Err(SqlError::InvalidParameter(
            "argument exceeds the maximum length of 255 bytes".into(),
        ));
    }
    if max_output > MAX_METAPHONE_STRLEN as i64 {
        return Err(SqlError::InvalidParameter(
            "output exceeds the maximum length of 255 bytes".into(),
        ));
    }
    if max_output <= 0 {
        return Err(SqlError::InvalidParameter(
            "output cannot be empty string".into(),
        ));
    }
    Ok(metaphone_raw(input.as_bytes(), max_output as usize))
}

/// The metaphone transformation proper (`_metaphone`), a mechanical port of
/// the C: byte-based, ASCII-uppercasing on access, non-ASCII treated as
/// non-alphabetic word breaks.
fn metaphone_raw(word: &[u8], max_phonemes: usize) -> String {
    // Curr_Letter / Next_Letter / Look_Back_Letter: uppercased, NUL ('\0' →
    // 0) outside the word.
    let up = |i: usize| -> u8 { word.get(i).map_or(0, u8::to_ascii_uppercase) };
    // Look_Ahead_Letter(n): edge forward up to n bytes, stopping at the end.
    let look_ahead = |i: usize, n: usize| -> u8 { up(i + n.min(word.len() - i)) };

    let mut out: Vec<u8> = Vec::with_capacity(max_phonemes);
    let mut w = 0usize;

    // The first phoneme is processed specially; find the first letter.
    while w < word.len() && !word[w].is_ascii_alphabetic() {
        w += 1;
    }
    if w == word.len() {
        return String::new();
    }
    match up(w) {
        // AE becomes E; other initial vowels are preserved.
        b'A' => {
            if up(w + 1) == b'E' {
                out.push(b'E');
                w += 2;
            } else {
                out.push(b'A');
                w += 1;
            }
        }
        // [GKP]N becomes N.
        b'G' | b'K' | b'P' if up(w + 1) == b'N' => {
            out.push(b'N');
            w += 2;
        }
        // WH becomes H, WR becomes R, W stays before a vowel.
        b'W' => {
            if up(w + 1) == b'H' || up(w + 1) == b'R' {
                out.push(up(w + 1));
                w += 2;
            } else if is_mvowel(up(w + 1)) {
                out.push(b'W');
                w += 2;
            }
        }
        // X becomes S.
        b'X' => {
            out.push(b'S');
            w += 1;
        }
        b'E' | b'I' | b'O' | b'U' => {
            out.push(up(w));
            w += 1;
        }
        _ => {}
    }

    // On to the metaphoning.
    while w < word.len() && out.len() < max_phonemes {
        // Letters consumed here beyond the current one (multi-letter rules).
        let mut skip = 0usize;
        let c = up(w);
        let prev = if w >= 1 { up(w - 1) } else { 0 };

        // Ignore non-alphas; drop duplicates, except CC.
        if !c.is_ascii_alphabetic() || (c == prev && c != b'C') {
            w += 1;
            continue;
        }

        let next = up(w + 1);
        let after_next = if next != 0 { up(w + 2) } else { 0 };
        match c {
            // B unless in -MB.
            b'B' if prev != b'M' => {
                out.push(b'B');
            }
            // 'sh' in -CIA- or -CH- (but K in CHR- / SCH-); S in C[IEY]
            // (dropped in SC[IEY]); else K.
            b'C' => {
                if makes_soft(next) {
                    if after_next == b'A' && next == b'I' {
                        out.push(SH);
                    } else if prev == b'S' {
                        // dropped
                    } else {
                        out.push(b'S');
                    }
                } else if next == b'H' {
                    if after_next == b'R' || prev == b'S' {
                        out.push(b'K'); // Christ, school
                    } else {
                        out.push(SH);
                    }
                    skip += 1;
                } else {
                    out.push(b'K');
                }
            }
            // J in -DG[EIY]-, else T.
            b'D' => {
                if next == b'G' && makes_soft(after_next) {
                    out.push(b'J');
                    skip += 1;
                } else {
                    out.push(b'T');
                }
            }
            // GH → F unless B/D/H nearby makes it silent; GN(ED) dropped;
            // soft G → J (but not GG); else K.
            b'G' => {
                if next == b'H' {
                    let back3 = if w >= 3 { up(w - 3) } else { 0 };
                    let back4 = if w >= 4 { up(w - 4) } else { 0 };
                    if !(no_gh_to_f(back3) || back4 == b'H') {
                        out.push(b'F');
                        skip += 1;
                    }
                    // else silent
                } else if next == b'N' {
                    if !after_next.is_ascii_alphabetic()
                        || (after_next == b'E' && look_ahead(w, 3) == b'D')
                    {
                        // dropped (-GN, -GNED)
                    } else {
                        out.push(b'K');
                    }
                } else if makes_soft(next) && prev != b'G' {
                    out.push(b'J');
                } else {
                    out.push(b'K');
                }
            }
            // H before a vowel and not after C/G/P/S/T.
            b'H' if is_mvowel(next) && !affects_h(prev) => {
                out.push(b'H');
            }
            // Dropped after C, else K.
            b'K' if prev != b'C' => {
                out.push(b'K');
            }
            // F before H, else P.
            b'P' => {
                if next == b'H' {
                    out.push(b'F');
                } else {
                    out.push(b'P');
                }
            }
            b'Q' => out.push(b'K'),
            // 'sh' in -SH-, -SIO-, -SIA-, -SCHW-; else S.
            b'S' => {
                if next == b'I' && (after_next == b'O' || after_next == b'A') {
                    out.push(SH);
                } else if next == b'H' {
                    out.push(SH);
                    skip += 1;
                } else if next == b'C' && look_ahead(w, 2) == b'H' && look_ahead(w, 3) == b'W' {
                    out.push(SH);
                    skip += 2;
                } else {
                    out.push(b'S');
                }
            }
            // 'sh' in -TIA-/-TIO-; 'th' before H; else T.
            b'T' => {
                if next == b'I' && (after_next == b'O' || after_next == b'A') {
                    out.push(SH);
                } else if next == b'H' {
                    out.push(TH);
                    skip += 1;
                } else {
                    out.push(b'T');
                }
            }
            b'V' => out.push(b'F'),
            // W before a vowel, else dropped.
            b'W' if is_mvowel(next) => {
                out.push(b'W');
            }
            // KS.
            b'X' => {
                out.push(b'K');
                if out.len() < max_phonemes {
                    out.push(b'S');
                }
            }
            // Y before a vowel.
            b'Y' if is_mvowel(next) => {
                out.push(b'Y');
            }
            b'Z' => out.push(b'S'),
            // Passed through unchanged.
            b'F' | b'J' | b'L' | b'M' | b'N' | b'R' => out.push(c),
            _ => {}
        }
        w += 1 + skip;
    }

    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Double Metaphone (port of contrib/fuzzystrmatch/dmetaphone.c, from the
// Text::DoubleMetaphone perl module, Lawrence Philips' 2000 algorithm)
// ---------------------------------------------------------------------------

/// The word under transformation: ASCII-uppercased bytes padded with 5 spaces
/// (like the C, which appends `"     "` so rules may index past the end).
struct DmWord {
    /// Uppercased input + padding.
    buf: Vec<u8>,
    /// Length *before* padding, in bytes.
    length: isize,
    /// `length - 1`.
    last: isize,
}

impl DmWord {
    fn new(input: &str) -> Self {
        // PostgreSQL uppercases via the collation; in a UTF-8 database every
        // non-ASCII byte stays >= 0x80 and falls through the dispatch switch,
        // so ASCII-uppercasing bytes reproduces its behaviour.
        let mut buf: Vec<u8> = input.bytes().map(|b| b.to_ascii_uppercase()).collect();
        let length = buf.len() as isize;
        buf.extend_from_slice(b"     ");
        DmWord {
            buf,
            length,
            last: length - 1,
        }
    }

    /// `GetAt`: NUL outside the padded buffer.
    fn at(&self, pos: isize) -> u8 {
        if pos < 0 || pos as usize >= self.buf.len() {
            0
        } else {
            self.buf[pos as usize]
        }
    }

    /// `IsVowel` (double-metaphone vowels include Y).
    fn vowel(&self, pos: isize) -> bool {
        matches!(self.at(pos), b'A' | b'E' | b'I' | b'O' | b'U' | b'Y')
    }

    /// `StringAt`: does any of `pats` (each exactly `len` bytes) occur at
    /// `start`? Matches may extend into the space padding, like the C.
    fn string_at(&self, start: isize, len: usize, pats: &[&[u8]]) -> bool {
        if start < 0 || start as usize >= self.buf.len() {
            return false;
        }
        let start = start as usize;
        let Some(window) = self.buf.get(start..start + len) else {
            return false;
        };
        pats.contains(&window)
    }

    /// `SlavoGermanic`.
    fn slavo_germanic(&self) -> bool {
        let has = |pat: &[u8]| self.buf.windows(pat.len()).any(|win| win == pat);
        has(b"W") || has(b"K") || has(b"CZ") || has(b"WITZ")
    }
}

/// Both Double Metaphone codes (primary, alternate), each at most 4 bytes.
#[allow(clippy::too_many_lines)] // one switch arm per letter, like the C original
fn double_metaphone(input: &str) -> (String, String) {
    let word = DmWord::new(input);
    let mut primary: Vec<u8> = Vec::new();
    let mut secondary: Vec<u8> = Vec::new();
    let mut current: isize = 0;
    let length = word.length;
    let last = word.last;

    let add = |p: &mut Vec<u8>, s: &[u8]| p.extend_from_slice(s);

    // Skip a silent first letter in GN-, KN-, PN-, WR-, PS-.
    if word.string_at(0, 2, &[b"GN", b"KN", b"PN", b"WR", b"PS"]) {
        current += 1;
    }

    // Initial X is pronounced Z, which maps to S ('Xavier').
    if word.at(0) == b'X' {
        add(&mut primary, b"S");
        add(&mut secondary, b"S");
        current += 1;
    }

    while primary.len() < 4 || secondary.len() < 4 {
        if current >= length {
            break;
        }
        match word.at(current) {
            b'A' | b'E' | b'I' | b'O' | b'U' | b'Y' => {
                if current == 0 {
                    // All initial vowels map to A.
                    add(&mut primary, b"A");
                    add(&mut secondary, b"A");
                }
                current += 1;
            }

            b'B' => {
                // "-mb" as in "dumb" was already skipped over (see M).
                add(&mut primary, b"P");
                add(&mut secondary, b"P");
                current += if word.at(current + 1) == b'B' { 2 } else { 1 };
            }

            0xC7 => {
                // C with cedilla (single-byte encodings).
                add(&mut primary, b"S");
                add(&mut secondary, b"S");
                current += 1;
            }

            b'C' => {
                // Various germanic: -ACH- with a consonant before and no
                // front vowel after ('macher').
                if current > 1
                    && !word.vowel(current - 2)
                    && word.string_at(current - 1, 3, &[b"ACH"])
                    && (word.at(current + 2) != b'I'
                        && (word.at(current + 2) != b'E'
                            || word.string_at(current - 2, 6, &[b"BACHER", b"MACHER"])))
                {
                    add(&mut primary, b"K");
                    add(&mut secondary, b"K");
                    current += 2;
                } else if current == 0 && word.string_at(current, 6, &[b"CAESAR"]) {
                    add(&mut primary, b"S");
                    add(&mut secondary, b"S");
                    current += 2;
                } else if word.string_at(current, 4, &[b"CHIA"]) {
                    // Italian 'chianti'.
                    add(&mut primary, b"K");
                    add(&mut secondary, b"K");
                    current += 2;
                } else if word.string_at(current, 2, &[b"CH"]) {
                    if current > 0 && word.string_at(current, 4, &[b"CHAE"]) {
                        // 'Michael'.
                        add(&mut primary, b"K");
                        add(&mut secondary, b"X");
                    } else if current == 0
                        && (word.string_at(current + 1, 5, &[b"HARAC", b"HARIS"])
                            || word.string_at(current + 1, 3, &[b"HOR", b"HYM", b"HIA", b"HEM"]))
                        && !word.string_at(0, 5, &[b"CHORE"])
                    {
                        // Greek roots: 'chemistry', 'chorus'.
                        add(&mut primary, b"K");
                        add(&mut secondary, b"K");
                    } else if (word.string_at(0, 4, &[b"VAN ", b"VON "])
                        || word.string_at(0, 3, &[b"SCH"]))
                        // 'architect' but not 'arch'; 'orchestra', 'orchid'
                        || word.string_at(current - 2, 6, &[b"ORCHES", b"ARCHIT", b"ORCHID"])
                        || word.string_at(current + 2, 1, &[b"T", b"S"])
                        || ((word.string_at(current - 1, 1, &[b"A", b"O", b"U", b"E"])
                            || current == 0)
                            // 'wachtler', 'wechsler', but not 'tichner'
                            && word.string_at(
                                current + 2,
                                1,
                                &[b"L", b"R", b"N", b"M", b"B", b"H", b"F", b"V", b"W", b" "],
                            ))
                    {
                        add(&mut primary, b"K");
                        add(&mut secondary, b"K");
                    } else if current > 0 {
                        if word.string_at(0, 2, &[b"MC"]) {
                            // 'McHugh'.
                            add(&mut primary, b"K");
                            add(&mut secondary, b"K");
                        } else {
                            add(&mut primary, b"X");
                            add(&mut secondary, b"K");
                        }
                    } else {
                        add(&mut primary, b"X");
                        add(&mut secondary, b"X");
                    }
                    current += 2;
                } else if word.string_at(current, 2, &[b"CZ"])
                    && !word.string_at(current - 2, 4, &[b"WICZ"])
                {
                    // 'czerny'.
                    add(&mut primary, b"S");
                    add(&mut secondary, b"X");
                    current += 2;
                } else if word.string_at(current + 1, 3, &[b"CIA"]) {
                    // 'focaccia'.
                    add(&mut primary, b"X");
                    add(&mut secondary, b"X");
                    current += 3;
                } else if word.string_at(current, 2, &[b"CC"])
                    && !(current == 1 && word.at(0) == b'M')
                {
                    // Double C, but not 'McClellan'.
                    if word.string_at(current + 2, 1, &[b"I", b"E", b"H"])
                        && !word.string_at(current + 2, 2, &[b"HU"])
                    {
                        // 'bellocchio' but not 'bacchus'.
                        if (current == 1 && word.at(current - 1) == b'A')
                            || word.string_at(current - 1, 5, &[b"UCCEE", b"UCCES"])
                        {
                            // 'accident', 'accede', 'succeed'.
                            add(&mut primary, b"KS");
                            add(&mut secondary, b"KS");
                        } else {
                            // 'bacci', 'bertucci': other italian.
                            add(&mut primary, b"X");
                            add(&mut secondary, b"X");
                        }
                        current += 3;
                    } else {
                        // Pierce's rule.
                        add(&mut primary, b"K");
                        add(&mut secondary, b"K");
                        current += 2;
                    }
                } else if word.string_at(current, 2, &[b"CK", b"CG", b"CQ"]) {
                    add(&mut primary, b"K");
                    add(&mut secondary, b"K");
                    current += 2;
                } else if word.string_at(current, 2, &[b"CI", b"CE", b"CY"]) {
                    // Italian vs. english.
                    if word.string_at(current, 3, &[b"CIO", b"CIE", b"CIA"]) {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"X");
                    } else {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"S");
                    }
                    current += 2;
                } else {
                    add(&mut primary, b"K");
                    add(&mut secondary, b"K");
                    // Names like 'mac caffrey', 'mac gregor'.
                    if word.string_at(current + 1, 2, &[b" C", b" Q", b" G"]) {
                        current += 3;
                    } else if word.string_at(current + 1, 1, &[b"C", b"K", b"Q"])
                        && !word.string_at(current + 1, 2, &[b"CE", b"CI"])
                    {
                        current += 2;
                    } else {
                        current += 1;
                    }
                }
            }

            b'D' => {
                if word.string_at(current, 2, &[b"DG"]) {
                    if word.string_at(current + 2, 1, &[b"I", b"E", b"Y"]) {
                        // 'edge'.
                        add(&mut primary, b"J");
                        add(&mut secondary, b"J");
                        current += 3;
                    } else {
                        // 'edgar'.
                        add(&mut primary, b"TK");
                        add(&mut secondary, b"TK");
                        current += 2;
                    }
                } else if word.string_at(current, 2, &[b"DT", b"DD"]) {
                    add(&mut primary, b"T");
                    add(&mut secondary, b"T");
                    current += 2;
                } else {
                    add(&mut primary, b"T");
                    add(&mut secondary, b"T");
                    current += 1;
                }
            }

            b'F' => {
                current += if word.at(current + 1) == b'F' { 2 } else { 1 };
                add(&mut primary, b"F");
                add(&mut secondary, b"F");
            }

            b'G' => {
                if word.at(current + 1) == b'H' {
                    if current > 0 && !word.vowel(current - 1) {
                        add(&mut primary, b"K");
                        add(&mut secondary, b"K");
                        current += 2;
                    } else if current == 0 {
                        // 'ghislane', 'ghiradelli'.
                        if word.at(current + 2) == b'I' {
                            add(&mut primary, b"J");
                            add(&mut secondary, b"J");
                        } else {
                            add(&mut primary, b"K");
                            add(&mut secondary, b"K");
                        }
                        current += 2;
                    } else if (current > 1 && word.string_at(current - 2, 1, &[b"B", b"H", b"D"]))
                        // 'bough'
                        || (current > 2 && word.string_at(current - 3, 1, &[b"B", b"H", b"D"]))
                        // 'broughton'
                        || (current > 3 && word.string_at(current - 4, 1, &[b"B", b"H"]))
                    {
                        // Parker's rule (with further refinements): 'hugh'.
                        current += 2;
                    } else {
                        // 'laugh', 'McLaughlin', 'cough', 'gough', 'rough'.
                        if current > 2
                            && word.at(current - 1) == b'U'
                            && word.string_at(current - 3, 1, &[b"C", b"G", b"L", b"R", b"T"])
                        {
                            add(&mut primary, b"F");
                            add(&mut secondary, b"F");
                        } else if current > 0 && word.at(current - 1) != b'I' {
                            add(&mut primary, b"K");
                            add(&mut secondary, b"K");
                        }
                        current += 2;
                    }
                } else if word.at(current + 1) == b'N' {
                    if current == 1 && word.vowel(0) && !word.slavo_germanic() {
                        add(&mut primary, b"KN");
                        add(&mut secondary, b"N");
                    } else if !word.string_at(current + 2, 2, &[b"EY"])
                        && word.at(current + 1) != b'Y'
                        && !word.slavo_germanic()
                    {
                        // Not 'cagney'.
                        add(&mut primary, b"N");
                        add(&mut secondary, b"KN");
                    } else {
                        add(&mut primary, b"KN");
                        add(&mut secondary, b"KN");
                    }
                    current += 2;
                } else if word.string_at(current + 1, 2, &[b"LI"]) && !word.slavo_germanic() {
                    // 'tagliaro'.
                    add(&mut primary, b"KL");
                    add(&mut secondary, b"L");
                    current += 2;
                } else if current == 0
                    && (word.at(current + 1) == b'Y'
                        || word.string_at(
                            current + 1,
                            2,
                            &[
                                b"ES", b"EP", b"EB", b"EL", b"EY", b"IB", b"IL", b"IN", b"IE",
                                b"EI", b"ER",
                            ],
                        ))
                {
                    // -ges-, -gep-, -gel-, -gie- at beginning.
                    add(&mut primary, b"K");
                    add(&mut secondary, b"J");
                    current += 2;
                } else if (word.string_at(current + 1, 2, &[b"ER"]) || word.at(current + 1) == b'Y')
                    && !word.string_at(0, 6, &[b"DANGER", b"RANGER", b"MANGER"])
                    && !word.string_at(current - 1, 1, &[b"E", b"I"])
                    && !word.string_at(current - 1, 3, &[b"RGY", b"OGY"])
                {
                    // -ger-, -gy-.
                    add(&mut primary, b"K");
                    add(&mut secondary, b"J");
                    current += 2;
                } else if word.string_at(current + 1, 1, &[b"E", b"I", b"Y"])
                    || word.string_at(current - 1, 4, &[b"AGGI", b"OGGI"])
                {
                    // Italian 'biaggi'.
                    if word.string_at(0, 4, &[b"VAN ", b"VON "])
                        || word.string_at(0, 3, &[b"SCH"])
                        || word.string_at(current + 1, 2, &[b"ET"])
                    {
                        // Obvious germanic.
                        add(&mut primary, b"K");
                        add(&mut secondary, b"K");
                    } else if word.string_at(current + 1, 4, &[b"IER "]) {
                        // Always soft if french ending.
                        add(&mut primary, b"J");
                        add(&mut secondary, b"J");
                    } else {
                        add(&mut primary, b"J");
                        add(&mut secondary, b"K");
                    }
                    current += 2;
                } else {
                    current += if word.at(current + 1) == b'G' { 2 } else { 1 };
                    add(&mut primary, b"K");
                    add(&mut secondary, b"K");
                }
            }

            b'H' => {
                // Only keep if first & before vowel, or between two vowels.
                if (current == 0 || word.vowel(current - 1)) && word.vowel(current + 1) {
                    add(&mut primary, b"H");
                    add(&mut secondary, b"H");
                    current += 2;
                } else {
                    // Also takes care of HH.
                    current += 1;
                }
            }

            b'J' => {
                // Obvious spanish: 'jose', 'san jacinto'.
                if word.string_at(current, 4, &[b"JOSE"]) || word.string_at(0, 4, &[b"SAN "]) {
                    if (current == 0 && word.at(current + 4) == b' ')
                        || word.string_at(0, 4, &[b"SAN "])
                    {
                        add(&mut primary, b"H");
                        add(&mut secondary, b"H");
                    } else {
                        add(&mut primary, b"J");
                        add(&mut secondary, b"H");
                    }
                    current += 1;
                } else {
                    if current == 0 && !word.string_at(current, 4, &[b"JOSE"]) {
                        // Yankelovich / Jankelowicz.
                        add(&mut primary, b"J");
                        add(&mut secondary, b"A");
                    } else if word.vowel(current - 1)
                        && !word.slavo_germanic()
                        && (word.at(current + 1) == b'A' || word.at(current + 1) == b'O')
                    {
                        // Spanish pronunciation: 'bajador'.
                        add(&mut primary, b"J");
                        add(&mut secondary, b"H");
                    } else if current == last {
                        add(&mut primary, b"J");
                    } else if !word.string_at(
                        current + 1,
                        1,
                        &[b"L", b"T", b"K", b"S", b"N", b"M", b"B", b"Z"],
                    ) && !word.string_at(current - 1, 1, &[b"S", b"K", b"L"])
                    {
                        add(&mut primary, b"J");
                        add(&mut secondary, b"J");
                    }
                    current += if word.at(current + 1) == b'J' { 2 } else { 1 };
                }
            }

            b'K' => {
                current += if word.at(current + 1) == b'K' { 2 } else { 1 };
                add(&mut primary, b"K");
                add(&mut secondary, b"K");
            }

            b'L' => {
                if word.at(current + 1) == b'L' {
                    // Spanish: 'cabrillo', 'gallegos'.
                    if (current == length - 3
                        && word.string_at(current - 1, 4, &[b"ILLO", b"ILLA", b"ALLE"]))
                        || ((word.string_at(last - 1, 2, &[b"AS", b"OS"])
                            || word.string_at(last, 1, &[b"A", b"O"]))
                            && word.string_at(current - 1, 4, &[b"ALLE"]))
                    {
                        add(&mut primary, b"L");
                        current += 2;
                        continue;
                    }
                    current += 2;
                } else {
                    current += 1;
                }
                add(&mut primary, b"L");
                add(&mut secondary, b"L");
            }

            b'M' => {
                if (word.string_at(current - 1, 3, &[b"UMB"])
                    && (current + 1 == last || word.string_at(current + 2, 2, &[b"ER"])))
                    // 'dumb', 'thumb'
                    || word.at(current + 1) == b'M'
                {
                    current += 2;
                } else {
                    current += 1;
                }
                add(&mut primary, b"M");
                add(&mut secondary, b"M");
            }

            b'N' => {
                current += if word.at(current + 1) == b'N' { 2 } else { 1 };
                add(&mut primary, b"N");
                add(&mut secondary, b"N");
            }

            0xD1 => {
                // N with tilde (single-byte encodings).
                current += 1;
                add(&mut primary, b"N");
                add(&mut secondary, b"N");
            }

            b'P' => {
                if word.at(current + 1) == b'H' {
                    add(&mut primary, b"F");
                    add(&mut secondary, b"F");
                    current += 2;
                } else {
                    // Also account for 'campbell', 'raspberry'.
                    current += if word.string_at(current + 1, 1, &[b"P", b"B"]) {
                        2
                    } else {
                        1
                    };
                    add(&mut primary, b"P");
                    add(&mut secondary, b"P");
                }
            }

            b'Q' => {
                current += if word.at(current + 1) == b'Q' { 2 } else { 1 };
                add(&mut primary, b"K");
                add(&mut secondary, b"K");
            }

            b'R' => {
                // French: 'rogier', but exclude 'hochmeier'.
                if current == last
                    && !word.slavo_germanic()
                    && word.string_at(current - 2, 2, &[b"IE"])
                    && !word.string_at(current - 4, 2, &[b"ME", b"MA"])
                {
                    add(&mut secondary, b"R");
                } else {
                    add(&mut primary, b"R");
                    add(&mut secondary, b"R");
                }
                current += if word.at(current + 1) == b'R' { 2 } else { 1 };
            }

            b'S' => {
                if word.string_at(current - 1, 3, &[b"ISL", b"YSL"]) {
                    // Silent: 'island', 'isle', 'carlisle', 'carlysle'.
                    current += 1;
                } else if current == 0 && word.string_at(current, 5, &[b"SUGAR"]) {
                    // Special case 'sugar-'.
                    add(&mut primary, b"X");
                    add(&mut secondary, b"S");
                    current += 1;
                } else if word.string_at(current, 2, &[b"SH"]) {
                    // Germanic.
                    if word.string_at(current + 1, 4, &[b"HEIM", b"HOEK", b"HOLM", b"HOLZ"]) {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"S");
                    } else {
                        add(&mut primary, b"X");
                        add(&mut secondary, b"X");
                    }
                    current += 2;
                } else if word.string_at(current, 3, &[b"SIO", b"SIA"])
                    || word.string_at(current, 4, &[b"SIAN"])
                {
                    // Italian & armenian.
                    if word.slavo_germanic() {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"S");
                    } else {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"X");
                    }
                    current += 3;
                } else if (current == 0
                    && word.string_at(current + 1, 1, &[b"M", b"N", b"L", b"W"]))
                    || word.string_at(current + 1, 1, &[b"Z"])
                {
                    // German & anglicisations: 'smith' matches 'schmidt',
                    // 'snider' matches 'schneider'; also -sz- in slavic.
                    add(&mut primary, b"S");
                    add(&mut secondary, b"X");
                    current += if word.string_at(current + 1, 1, &[b"Z"]) {
                        2
                    } else {
                        1
                    };
                } else if word.string_at(current, 2, &[b"SC"]) {
                    // Schlesinger's rule.
                    if word.at(current + 2) == b'H' {
                        // Dutch origin: 'school', 'schooner'.
                        if word.string_at(
                            current + 3,
                            2,
                            &[b"OO", b"ER", b"EN", b"UY", b"ED", b"EM"],
                        ) {
                            // 'schermerhorn', 'schenker'.
                            if word.string_at(current + 3, 2, &[b"ER", b"EN"]) {
                                add(&mut primary, b"X");
                                add(&mut secondary, b"SK");
                            } else {
                                add(&mut primary, b"SK");
                                add(&mut secondary, b"SK");
                            }
                        } else if current == 0 && !word.vowel(3) && word.at(3) != b'W' {
                            add(&mut primary, b"X");
                            add(&mut secondary, b"S");
                        } else {
                            add(&mut primary, b"X");
                            add(&mut secondary, b"X");
                        }
                    } else if word.string_at(current + 2, 1, &[b"I", b"E", b"Y"]) {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"S");
                    } else {
                        add(&mut primary, b"SK");
                        add(&mut secondary, b"SK");
                    }
                    current += 3;
                } else {
                    // French: 'resnais', 'artois'.
                    if current == last && word.string_at(current - 2, 2, &[b"AI", b"OI"]) {
                        add(&mut secondary, b"S");
                    } else {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"S");
                    }
                    current += if word.string_at(current + 1, 1, &[b"S", b"Z"]) {
                        2
                    } else {
                        1
                    };
                }
            }

            b'T' => {
                // -TION-, -TIA-, -TCH- all read as 'sh' (two rules in the C).
                if word.string_at(current, 4, &[b"TION"])
                    || word.string_at(current, 3, &[b"TIA", b"TCH"])
                {
                    add(&mut primary, b"X");
                    add(&mut secondary, b"X");
                    current += 3;
                } else if word.string_at(current, 2, &[b"TH"])
                    || word.string_at(current, 3, &[b"TTH"])
                {
                    // Special case 'thomas', 'thames', or germanic.
                    if word.string_at(current + 2, 2, &[b"OM", b"AM"])
                        || word.string_at(0, 4, &[b"VAN ", b"VON "])
                        || word.string_at(0, 3, &[b"SCH"])
                    {
                        add(&mut primary, b"T");
                        add(&mut secondary, b"T");
                    } else {
                        add(&mut primary, b"0");
                        add(&mut secondary, b"T");
                    }
                    current += 2;
                } else {
                    current += if word.string_at(current + 1, 1, &[b"T", b"D"]) {
                        2
                    } else {
                        1
                    };
                    add(&mut primary, b"T");
                    add(&mut secondary, b"T");
                }
            }

            b'V' => {
                current += if word.at(current + 1) == b'V' { 2 } else { 1 };
                add(&mut primary, b"F");
                add(&mut secondary, b"F");
            }

            b'W' => {
                // Can also be in the middle of a word.
                if word.string_at(current, 2, &[b"WR"]) {
                    add(&mut primary, b"R");
                    add(&mut secondary, b"R");
                    current += 2;
                } else {
                    if current == 0
                        && (word.vowel(current + 1) || word.string_at(current, 2, &[b"WH"]))
                    {
                        if word.vowel(current + 1) {
                            // 'Wasserman' should match 'Vasserman'.
                            add(&mut primary, b"A");
                            add(&mut secondary, b"F");
                        } else {
                            // Need 'Uomo' to match 'Womo'.
                            add(&mut primary, b"A");
                            add(&mut secondary, b"A");
                        }
                    }
                    if (current == last && word.vowel(current - 1))
                        || word.string_at(current - 1, 5, &[b"EWSKI", b"EWSKY", b"OWSKI", b"OWSKY"])
                        || word.string_at(0, 3, &[b"SCH"])
                    {
                        // 'Arnow' should match 'Arnoff'.
                        add(&mut secondary, b"F");
                        current += 1;
                    } else if word.string_at(current, 4, &[b"WICZ", b"WITZ"]) {
                        // Polish: 'filipowicz'.
                        add(&mut primary, b"TS");
                        add(&mut secondary, b"FX");
                        current += 4;
                    } else {
                        // Else skip it.
                        current += 1;
                    }
                }
            }

            b'X' => {
                // French: 'breaux'.
                if !(current == last
                    && (word.string_at(current - 3, 3, &[b"IAU", b"EAU"])
                        || word.string_at(current - 2, 2, &[b"AU", b"OU"])))
                {
                    add(&mut primary, b"KS");
                    add(&mut secondary, b"KS");
                }
                current += if word.string_at(current + 1, 1, &[b"C", b"X"]) {
                    2
                } else {
                    1
                };
            }

            b'Z' => {
                if word.at(current + 1) == b'H' {
                    // Chinese pinyin: 'zhao'.
                    add(&mut primary, b"J");
                    add(&mut secondary, b"J");
                    current += 2;
                } else {
                    if word.string_at(current + 1, 2, &[b"ZO", b"ZI", b"ZA"])
                        || (word.slavo_germanic() && current > 0 && word.at(current - 1) != b'T')
                    {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"TS");
                    } else {
                        add(&mut primary, b"S");
                        add(&mut secondary, b"S");
                    }
                    current += if word.at(current + 1) == b'Z' { 2 } else { 1 };
                }
            }

            _ => current += 1,
        }
    }

    primary.truncate(4);
    secondary.truncate(4);
    (
        String::from_utf8_lossy(&primary).into_owned(),
        String::from_utf8_lossy(&secondary).into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    // Every expected value in this module was produced by a live PostgreSQL
    // 16.13 server with fuzzystrmatch 1.2 installed.

    fn invoke(name: &str, args: &[SqlValue]) -> Result<SqlValue> {
        let vars = RefCell::new(HashMap::new());
        let ctx = ExtCtx {
            now: chrono::Utc::now(),
            vars: &vars,
        };
        call(&ctx, name, args)
    }

    fn t(s: &str) -> SqlValue {
        SqlValue::Text(s.into())
    }

    fn i(v: i32) -> SqlValue {
        SqlValue::Int4(v)
    }

    fn text(name: &str, args: &[SqlValue]) -> String {
        match invoke(name, args) {
            Ok(SqlValue::Text(s)) => s,
            other => panic!("{name}: expected text, got {other:?}"),
        }
    }

    fn int(name: &str, args: &[SqlValue]) -> i32 {
        match invoke(name, args) {
            Ok(SqlValue::Int4(v)) => v,
            other => panic!("{name}: expected int4, got {other:?}"),
        }
    }

    #[test]
    fn def_metadata() {
        assert_eq!(DEF.name, "fuzzystrmatch");
        assert_eq!(DEF.default_version, "1.2");
        assert!(DEF.trusted);
        assert!(DEF.requires.is_empty());
        assert!(DEF.types.is_empty());
        assert!(DEF.gucs.is_empty());
        assert!(DEF.call.is_some());
        for f in [
            "levenshtein",
            "levenshtein_less_equal",
            "soundex",
            "difference",
            "metaphone",
            "dmetaphone",
            "dmetaphone_alt",
        ] {
            assert!(DEF.functions.contains(&f), "missing {f}");
        }
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(int("levenshtein", &[t("kitten"), t("sitting")]), 3);
        assert_eq!(int("levenshtein", &[t("GUMBO"), t("GAMBOL")]), 2);
        assert_eq!(int("levenshtein", &[t(""), t("abc")]), 3);
        assert_eq!(int("levenshtein", &[t("abc"), t("")]), 3);
        assert_eq!(int("levenshtein", &[t("same"), t("same")]), 0);
        // Character-based, not byte-based.
        assert_eq!(int("levenshtein", &[t("café"), t("cafe")]), 1);
        assert_eq!(int("levenshtein", &[t("日本語"), t("日本")]), 1);
        assert_eq!(int("levenshtein", &[t("ééé"), t("eee")]), 3);
    }

    #[test]
    fn levenshtein_with_costs() {
        let lev = |a: &str, b: &str, ins: i32, del: i32, sub: i32| {
            int("levenshtein", &[t(a), t(b), i(ins), i(del), i(sub)])
        };
        assert_eq!(lev("a", "b", 10, 10, 10), 10); // one substitution
        assert_eq!(lev("GUMBO", "GAMBOL", 2, 1, 1), 3); // sub + insert
        assert_eq!(lev("ab", "ba", 3, 5, 7), 8); // insert + delete beat 2 subs
        assert_eq!(lev("foo", "four", 10, 1, 10), 20);
        // Int8 cost arguments coerce like any integer.
        assert_eq!(
            int(
                "levenshtein",
                &[
                    t("a"),
                    t("b"),
                    SqlValue::Int8(10),
                    SqlValue::Int8(10),
                    SqlValue::Int8(10)
                ]
            ),
            10
        );
    }

    #[test]
    fn levenshtein_length_limit() {
        let long_a = "a".repeat(256);
        let err = invoke("levenshtein", &[t(&long_a), t("b")]).unwrap_err();
        assert_eq!(
            err,
            SqlError::InvalidParameter(
                "argument exceeds the maximum length of 255 characters".into()
            )
        );
        let err = invoke("levenshtein_less_equal", &[t(&long_a), t("b"), i(3)]).unwrap_err();
        assert!(matches!(err, SqlError::InvalidParameter(_)));

        // Exactly 255 characters is allowed.
        assert_eq!(
            int("levenshtein", &[t(&"a".repeat(255)), t(&"b".repeat(255))]),
            255
        );
        // The limit counts characters, not bytes (PG: 255 'é' pass).
        assert_eq!(int("levenshtein", &[t(&"é".repeat(255)), t("e")]), 255);
        // An empty argument short-circuits before the limit check (PG quirk).
        assert_eq!(int("levenshtein", &[t(""), t(&"a".repeat(300))]), 300);
        assert_eq!(int("levenshtein", &[t(&"a".repeat(300)), t("")]), 300);
    }

    #[test]
    fn levenshtein_less_equal_banded() {
        let lle = |a: &str, b: &str, max: i32| int("levenshtein_less_equal", &[t(a), t(b), i(max)]);
        // Real distance is 4; with max 2 PostgreSQL's band collapses at 3.
        assert_eq!(lle("extensive", "exhaustive", 2), 3);
        assert_eq!(lle("extensive", "exhaustive", 4), 4);
        assert_eq!(lle("kitten", "sitting", 10), 3);
        assert_eq!(lle("kitten", "sitting", 2), 3);
        assert_eq!(lle("kitten", "sitting", 0), 1);
        assert_eq!(lle("same", "same", 0), 0);
        // Length-difference pruning: real distance 6, PG reports max + 1.
        assert_eq!(lle("a", "abcdefg", 3), 4);
        // A negative bound disables the band: exact distance comes back.
        assert_eq!(lle("extensive", "exhaustive", -3), 4);
        // Cost-parameterised form.
        assert_eq!(
            int(
                "levenshtein_less_equal",
                &[t("ab"), t("ba"), i(3), i(5), i(7), i(10)]
            ),
            8
        );
    }

    #[test]
    fn soundex_matches_postgres() {
        for (input, code) in [
            ("Anne", "A500"),
            ("Ann", "A500"),
            ("Andrew", "A536"),
            ("Margaret", "M626"),
            ("hello world!", "H464"),
            ("Tymczak", "T522"),
            ("Pfister", "P236"),
            ("honeyman", "H555"),
            ("Ashcraft", "A226"),
            ("a", "A000"),
            ("  42Robert", "R163"), // leading non-letters are skipped
            ("", ""),
            ("123", ""), // no letters at all: empty, like PG
        ] {
            assert_eq!(text("soundex", &[t(input)]), code, "soundex({input:?})");
        }
        // citext arguments are accepted.
        assert_eq!(text("soundex", &[SqlValue::Citext("Anne".into())]), "A500");
    }

    #[test]
    fn difference_matches_postgres() {
        assert_eq!(int("difference", &[t("Anne"), t("Ann")]), 4);
        assert_eq!(int("difference", &[t("Anne"), t("Andrew")]), 2);
        assert_eq!(int("difference", &[t("Anne"), t("Margaret")]), 0);
        // Letterless soundex codes are all-zero buffers in PG, so two empty
        // inputs agree on every position and one empty input on none.
        assert_eq!(int("difference", &[t(""), t("")]), 4);
        assert_eq!(int("difference", &[t("Anne"), t("")]), 0);
        assert_eq!(int("difference", &[t(""), t("Anne")]), 0);
    }

    #[test]
    fn metaphone_matches_postgres() {
        let met = |s: &str, n: i32| text("metaphone", &[t(s), i(n)]);
        assert_eq!(met("GUMBO", 4), "KM");
        assert_eq!(met("phone", 10), "FN");
        // Note: PostgreSQL's metaphone renders TH as '0' — 'Thompson' is
        // "0MPSN", not the "TMSN" that PHP's variant of the algorithm emits.
        assert_eq!(met("Thompson", 10), "0MPSN");
        assert_eq!(met("Thomas", 10), "0MS");
        assert_eq!(met("school", 10), "SKL");
        assert_eq!(met("science", 10), "SNS");
        assert_eq!(met("Knight", 10), "NFT");
        assert_eq!(met("what", 10), "HT");
        assert_eq!(met("wright", 10), "RFT");
        assert_eq!(met("xylophone", 10), "SLFN");
        assert_eq!(met("aeon", 10), "EN");
        assert_eq!(met("judge", 10), "JJ");
        assert_eq!(met("McCartney", 10), "MKKRTN");
        assert_eq!(met("schwa", 10), "XW");
        assert_eq!(met("caution", 10), "KXN");
        assert_eq!(met("Otto", 10), "OT");
        assert_eq!(met("hugh", 4), "HF");
        assert_eq!(met("ghost", 4), "FST");
        assert_eq!(met("Yes", 4), "YS");
        assert_eq!(met("yield", 4), "YLT");
        assert_eq!(met("box", 4), "BKS");
        assert_eq!(met("excel", 4), "EKSS");
        assert_eq!(met("a1b2c3", 10), "ABK"); // non-alphas break phonemes
        assert_eq!(met("   ae   ", 10), "E");
        assert_eq!(met("", 4), "");
        assert_eq!(met("123 456", 10), "");
        // Truncation to the requested output length.
        assert_eq!(met("Christmas", 10), "KRSTMS");
        assert_eq!(met("Christmas", 4), "KRST");
    }

    #[test]
    fn metaphone_argument_validation() {
        let err = invoke("metaphone", &[t("abc"), i(0)]).unwrap_err();
        assert_eq!(
            err,
            SqlError::InvalidParameter("output cannot be empty string".into())
        );
        assert!(invoke("metaphone", &[t("abc"), i(-1)]).is_err());
        let err = invoke("metaphone", &[t("abc"), i(256)]).unwrap_err();
        assert_eq!(
            err,
            SqlError::InvalidParameter("output exceeds the maximum length of 255 bytes".into())
        );
        let err = invoke("metaphone", &[t(&"a".repeat(256)), i(4)]).unwrap_err();
        assert_eq!(
            err,
            SqlError::InvalidParameter("argument exceeds the maximum length of 255 bytes".into())
        );
        // Like PG, the empty string returns before any validation runs.
        assert_eq!(text("metaphone", &[t(""), i(0)]), "");
    }

    #[test]
    fn dmetaphone_matches_postgres() {
        let dm = |s: &str| (text("dmetaphone", &[t(s)]), text("dmetaphone_alt", &[t(s)]));
        let pair = |p: &str, a: &str| (p.to_string(), a.to_string());
        assert_eq!(dm("gumbo"), pair("KMP", "KMP"));
        assert_eq!(dm("metaphone"), pair("MTFN", "MTFN"));
        assert_eq!(dm("Schmidt"), pair("XMT", "SMT"));
        assert_eq!(dm("smith"), pair("SM0", "XMT"));
        assert_eq!(dm("Thompson"), pair("TMPS", "TMPS"));
        assert_eq!(dm("Xavier"), pair("SF", "SFR"));
        assert_eq!(dm("jose"), pair("HS", "HS"));
        assert_eq!(dm("filipowicz"), pair("FLPT", "FLPF"));
        assert_eq!(dm("Wasserman"), pair("ASRM", "FSRM"));
        assert_eq!(dm("breaux"), pair("PR", "PR"));
        assert_eq!(dm("zhao"), pair("J", "J"));
        assert_eq!(dm("focaccia"), pair("FKX", "FKX"));
        assert_eq!(dm("McHugh"), pair("MK", "MK"));
        assert_eq!(dm("island"), pair("ALNT", "ALNT"));
        assert_eq!(dm("sugar"), pair("XKR", "SKR"));
        assert_eq!(dm("cabrillo"), pair("KPRL", "KPR"));
        assert_eq!(dm("edge"), pair("AJ", "AJ"));
        assert_eq!(dm("edgar"), pair("ATKR", "ATKR"));
        assert_eq!(dm("dumb"), pair("TM", "TM"));
        assert_eq!(dm("thumb"), pair("0M", "TM"));
        assert_eq!(dm("campbell"), pair("KMPL", "KMPL"));
        assert_eq!(dm("raspberry"), pair("RSPR", "RSPR"));
        assert_eq!(dm("czerny"), pair("SRN", "XRN"));
        assert_eq!(dm("Yankelovich"), pair("ANKL", "ANKL"));
        assert_eq!(dm("Jankelowicz"), pair("JNKL", "ANKL"));
        assert_eq!(dm(""), pair("", ""));
        assert_eq!(dm("123"), pair("", ""));
    }

    #[test]
    fn null_arguments_yield_null() {
        let null = SqlValue::Null;
        for (name, args) in [
            ("levenshtein", vec![null.clone(), t("x")]),
            ("levenshtein", vec![t("x"), null.clone()]),
            ("levenshtein_less_equal", vec![t("x"), t("y"), null.clone()]),
            ("soundex", vec![null.clone()]),
            ("difference", vec![null.clone(), null.clone()]),
            ("metaphone", vec![t("x"), null.clone()]),
            ("metaphone", vec![null.clone(), i(4)]),
            ("dmetaphone", vec![null.clone()]),
            ("dmetaphone_alt", vec![null.clone()]),
        ] {
            assert!(
                matches!(invoke(name, &args), Ok(SqlValue::Null)),
                "{name} is STRICT"
            );
        }
    }

    #[test]
    fn dispatch_errors() {
        // A name outside the registry is an internal routing error.
        assert!(matches!(
            invoke("not_a_function", &[t("x")]),
            Err(SqlError::Internal(_))
        ));
        // Argument-count mismatches surface as unknown signatures.
        assert!(matches!(
            invoke("levenshtein", &[t("a"), t("b"), i(1)]),
            Err(SqlError::UndefinedFunction(_))
        ));
        assert!(matches!(
            invoke("levenshtein_less_equal", &[t("a"), t("b"), i(1), i(1)]),
            Err(SqlError::UndefinedFunction(_))
        ));
        assert!(matches!(
            invoke("soundex", &[t("a"), t("b")]),
            Err(SqlError::UndefinedFunction(_))
        ));
    }
}
