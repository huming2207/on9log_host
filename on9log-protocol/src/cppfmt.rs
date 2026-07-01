//! C++23 `std::format`-style format-string rendering, delegated to Rust's
//! `std::fmt` traits for value conversion.
//!
//! This renderer coexists with the printf renderer in [`crate::printf`]. The
//! dispatcher ([`crate::printf::render_format`]) routes a string here only when
//! it contains a C++23-style replacement field *and* no active printf
//! conversion, so mixed strings such as `"payload={} status=%d"` stay on the
//! printf path (matching the firmware's `format(printf)` annotation) instead of
//! being mis-assigned by `{}`.
//!
//! ## Why `std::fmt` plus a pre-processor
//!
//! `std::fmt` cannot parse a *runtime* format string: `format!`/`format_args!`
//! require a string literal, and `fmt::Arguments` has no public constructor for
//! dynamic specs. So we do what the user asked in two layers:
//!
//! 1. **Pre-processing / spec interpretation** — we parse the C++23 field
//!    grammar ourselves into a [`Spec`] and interpret the C++23 type character.
//!    Rust's format spec has no `d` (decimal-forced), `s` (string-forced), or
//!    `c` (char-from-codepoint) type characters, so those are handled here by
//!    picking the right Rust trait / conversion before padding is applied.
//! 2. **Value rendering via `std::fmt`** — the core digit/float/hex/pointer
//!    conversion is done with `std::fmt`'s `Display`, `LowerHex`, `UpperHex`,
//!    `Octal`, `Binary`, `LowerExp`, `UpperExp`, and `Pointer`-equivalent
//!    formatting (via `format!` with literal specs). Width, fill, align, sign,
//!    `#`, `0`, and precision are then applied around that core output.
//!
//! ## Dynamic width / precision
//!
//! Nested replacement fields are supported in the width and precision
//! positions: `{:{}}`, `{:.{}}`, `{0:{1}}`, `{:.{1}}`. The referenced argument
//! must be an integer. Auto arg-id assignment order within one field is
//! value → width → precision (C++23 rule). As with value arg-ids, mixing auto
//! and explicit indexing anywhere in the format string is an error.
//!
//! ## Supported C++23 type characters
//!
//! `b B c d e E f F g G o p s x X` plus empty (default). `{}` defaults follow
//! the wire argument type: `Arg::Str` -> string, `Arg::Ptr` -> `0x` address,
//! `Arg::U32`/`Arg::U64` -> unsigned decimal, `Arg::U64` with a float type ->
//! `f64` (bit-cast).
//!
//! ## Excluded by design (per project scope)
//!
//! `L` (locale), `a`/`A` (hex float), C++ chrono/tuple/range formatters are not
//! implemented. Using them yields a `<render error: ...>` marker so the log
//! line is still visible instead of silently misrouted.
//!
//! ## Known limitations
//!
//! - The wire carries no signedness and no float/int distinction for 64-bit
//!   values, so `{}` on an `Arg::U64` renders the raw bit pattern as an
//!   unsigned integer. Use explicit type chars (`{:f}`, `{:d}`) when the
//!   default is ambiguous; correcting `{}` defaults requires richer firmware
//!   wire metadata (e.g. separate signed/float type codes).
//! - `g`/`G` use Rust's shortest round-trip rendering (`{:?}`); C++ significant-
//!   digit precision for `g` is not exactly reproduced.
//! - `#` for floats (forced decimal point) is ignored.
//! - Rust's `LowerExp`/`UpperExp` use a 1-digit exponent (`e0`) versus C++'s
//!   `e+00`.

use crate::printf::Arg;

/// A parsed piece of a format string: either literal text or a replacement field.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// A literal text segment (no special handling).
    Literal(String),
    /// A parsed `{...}` replacement field.
    Field(Field),
}

/// A parsed C++23 replacement field with optional arg-id and spec.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    /// `None` means automatic arg-id (`{}`).
    arg_id: Option<usize>,
    /// The parsed format specification (the part after `:`).
    spec: Spec,
}

/// Fill-and-align direction for a C++23 format spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    /// Left-align (`<`).
    Left,
    /// Right-align (`>`).
    Right,
    /// Center-align (`^`).
    Center,
}

impl Align {
    /// Convert a character to an [`Align`]. Returns an error for non-align chars.
    fn from_char(c: char) -> Result<Self, String> {
        match c {
            '<' => Ok(Self::Left),
            '>' => Ok(Self::Right),
            '^' => Ok(Self::Center),
            _ => Err("bad align".into()),
        }
    }
}

/// A width or precision that may be a literal, a nested auto ref, or a nested
/// explicit ref. `None` means the component was absent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Count {
    /// The width/precision was not specified.
    #[default]
    None,
    /// A literal decimal value (e.g. `{:.5}`).
    Literal(usize),
    /// A nested auto-indexed replacement field (e.g. `{:{}}`).
    Auto,
    /// A nested explicit-indexed replacement field (e.g. `{0:{1}}`).
    Explicit(usize),
}

/// Parsed C++23 format spec (the text between `:` and `}`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Spec {
    /// Optional fill character (default is space).
    fill: Option<char>,
    /// Alignment direction (`<`, `>`, or `^`).
    align: Option<Align>,
    /// Sign mode (`+`, `-`, or ` `).
    sign: Option<char>,
    /// Whether the `#` (alternate form) flag is present.
    alt: bool,
    /// Whether the `0` (zero-pad) flag is present.
    zero: bool,
    /// Width specification (literal, auto, or explicit index).
    width: Count,
    /// Precision specification (literal, auto, or explicit index).
    precision: Count,
    /// The type character (`d`, `x`, `f`, `s`, etc.), or `None` for default.
    type_char: Option<char>,
}

/// True if `fmt` contains at least one unescaped C++23-style replacement field.
///
/// This is only the *field detector*. The dispatch decision in
/// [`crate::printf::render_format`] additionally requires that the string
/// contains no active printf conversion, so mixed strings are not misrouted.
/// A field-ish token is `{` followed by optional ASCII digits and then `:` or
/// `}`. This avoids flagging printf strings that happen to contain literal
/// braces such as `json {key=%d}` (the `k` after `{` is not a digit, `:`, or
/// `}`, so it is not field-ish).
pub fn looks_like_cpp(fmt: &str) -> bool {
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if i + 1 < chars.len() && chars[i + 1] == '{' {
                i += 2;
                continue;
            }
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == ':' || chars[j] == '}') {
                return true;
            }
            i += 1;
        } else if c == '}' {
            if i + 1 < chars.len() && chars[i + 1] == '}' {
                i += 2;
                continue;
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    false
}

/// Render a C++23-style format string against decoded wire arguments.
///
/// On any parse/format error the original format string is returned with a
/// `<render error: ...>` prefix so the log line is not lost, matching the
/// printf renderer's recovery behavior.
pub fn render(fmt: &str, args: &[Arg]) -> String {
    let pieces = match parse(fmt) {
        Ok(p) => p,
        Err(e) => return format!("<render error: {e}> {fmt}"),
    };

    // C++: auto and explicit arg-id indexing cannot be mixed anywhere in the
    // string, including nested width/precision refs. Absent width/precision
    // (and literal counts) do not participate in indexing and are ignored.
    let mut has_auto = false;
    let mut has_explicit = false;
    for p in &pieces {
        if let Piece::Field(f) = p {
            match f.arg_id {
                None => has_auto = true,
                Some(_) => has_explicit = true,
            }
            match &f.spec.width {
                Count::Auto => has_auto = true,
                Count::Explicit(_) => has_explicit = true,
                _ => {}
            }
            match &f.spec.precision {
                Count::Auto => has_auto = true,
                Count::Explicit(_) => has_explicit = true,
                _ => {}
            }
        }
    }
    if has_auto && has_explicit {
        return format!("<render error: mixed auto and explicit args> {fmt}");
    }

    let mut out = String::with_capacity(fmt.len() + args.len() * 4);
    let mut auto = 0usize;
    for p in pieces {
        match p {
            Piece::Literal(s) => out.push_str(&s),
            Piece::Field(f) => {
                // Arg-id assignment order within a field: value, width, precision.
                let value_idx = match f.arg_id {
                    Some(id) => id,
                    None => {
                        let id = auto;
                        auto += 1;
                        id
                    }
                };
                let width = match &f.spec.width {
                    Count::None => None,
                    Count::Literal(w) => Some(*w),
                    Count::Auto => {
                        let id = auto;
                        auto += 1;
                        match fetch_count(args, id) {
                            Ok(w) => Some(w),
                            Err(e) => return format!("<render error: {e}> {fmt}"),
                        }
                    }
                    Count::Explicit(id) => match fetch_count(args, *id) {
                        Ok(w) => Some(w),
                        Err(e) => return format!("<render error: {e}> {fmt}"),
                    },
                };
                let precision = match &f.spec.precision {
                    Count::None => None,
                    Count::Literal(p) => Some(*p),
                    Count::Auto => {
                        let id = auto;
                        auto += 1;
                        match fetch_count(args, id) {
                            Ok(p) => Some(p),
                            Err(e) => return format!("<render error: {e}> {fmt}"),
                        }
                    }
                    Count::Explicit(id) => match fetch_count(args, *id) {
                        Ok(p) => Some(p),
                        Err(e) => return format!("<render error: {e}> {fmt}"),
                    },
                };

                if value_idx >= args.len() {
                    return format!("<render error: missing arg {value_idx}> {fmt}");
                }
                match render_field(&args[value_idx], &f.spec, width, precision) {
                    Ok(s) => out.push_str(&s),
                    Err(e) => return format!("<render error: {e}> {fmt}"),
                }
            }
        }
    }
    out
}

/// Resolve a dynamic width/precision argument to a non-negative `usize`.
///
/// C++23 treats a negative dynamic width/precision as a format error, so a
/// 32-bit value whose signed interpretation is negative is rejected (this also
/// avoids a multi-gigabyte pad allocation from a stray `0xffffffff`).
fn fetch_count(args: &[Arg], idx: usize) -> Result<usize, String> {
    if idx >= args.len() {
        return Err(format!("missing width/precision arg {idx}"));
    }
    let signed: i64 = match &args[idx] {
        Arg::U32(v) => *v as i32 as i64,
        Arg::U64(v) => *v as i64,
        Arg::Ptr(v) => *v as i32 as i64,
        Arg::Str(_) => return Err("string used as width/precision".into()),
    };
    if signed < 0 {
        return Err("negative dynamic width/precision".into());
    }
    Ok(signed as usize)
}

/// Parse a C++23-style format string into a sequence of [`Piece`] values.
///
/// Handles `{{` / `}}` escapes, `{arg_id:spec}` replacement fields, and
/// nested brace-depth tracking for dynamic width/precision refs.
fn parse(fmt: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut lit = String::new();
    let mut chars = fmt.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '{' {
            if chars.peek() == Some(&'{') {
                chars.next();
                lit.push('{');
                continue;
            }
            if !lit.is_empty() {
                pieces.push(Piece::Literal(std::mem::take(&mut lit)));
            }
            let field = parse_field(&mut chars)?;
            pieces.push(Piece::Field(field));
        } else if c == '}' {
            if chars.peek() == Some(&'}') {
                chars.next();
                lit.push('}');
                continue;
            }
            return Err("unmatched '}'".into());
        } else {
            lit.push(c);
        }
    }
    if !lit.is_empty() {
        pieces.push(Piece::Literal(lit));
    }
    Ok(pieces)
}

/// Parse one replacement field body (between `{` and `}`).
///
/// Reads an optional arg-id (decimal digits), then either a `:` followed by a
/// spec string (with brace-depth tracking for nested refs) or a closing `}`.
fn parse_field<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
) -> Result<Field, String> {
    let mut id_str = String::new();
    while matches!(chars.peek(), Some(d) if d.is_ascii_digit()) {
        id_str.push(chars.next().unwrap());
    }
    let arg_id = if id_str.is_empty() {
        None
    } else {
        Some(
            id_str
                .parse::<usize>()
                .map_err(|_| "bad arg id".to_string())?,
        )
    };

    let spec = match chars.peek() {
        Some(':') => {
            chars.next();
            // Collect the spec body with brace-depth tracking so nested
            // replacement fields (`{:{}}`, `{0:{1}}`) are captured intact.
            let mut spec_str = String::new();
            let mut depth = 1usize;
            loop {
                match chars.next() {
                    Some('{') => {
                        depth += 1;
                        spec_str.push('{');
                    }
                    Some('}') => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        spec_str.push('}');
                    }
                    Some(c) => spec_str.push(c),
                    None => return Err("unterminated field".into()),
                }
            }
            parse_spec(&spec_str)?
        }
        Some('}') => {
            chars.next();
            Spec::default()
        }
        Some(_) => return Err("expected ':' or '}' in field".into()),
        None => return Err("unterminated field".into()),
    };

    Ok(Field { arg_id, spec })
}

/// Parse a C++23 format specification string (the part between `:` and `}`)
/// into a [`Spec`] struct.
///
/// Handles fill-and-align, sign, `#` flag, `0` flag, width, precision, and
/// the type character. Returns an error for unsupported or malformed specs.
fn parse_spec(s: &str) -> Result<Spec, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut spec = Spec::default();

    // fill-and-align
    if i < chars.len() && is_align(chars[i]) {
        spec.align = Some(Align::from_char(chars[i])?);
        i += 1;
    } else if i + 1 < chars.len() && is_align(chars[i + 1]) {
        let f = chars[i];
        if f == '{' || f == '}' {
            return Err("invalid fill character".into());
        }
        spec.fill = Some(f);
        spec.align = Some(Align::from_char(chars[i + 1])?);
        i += 2;
    }

    // sign
    if i < chars.len() && matches!(chars[i], '+' | '-' | ' ') {
        spec.sign = Some(chars[i]);
        i += 1;
    }
    // '#'
    if i < chars.len() && chars[i] == '#' {
        spec.alt = true;
        i += 1;
    }
    // '0' zero pad
    if i < chars.len() && chars[i] == '0' {
        spec.zero = true;
        i += 1;
    }
    // width — literal digits or a nested replacement field.
    spec.width = parse_count(&chars, &mut i)?;
    // precision — '.' then literal digits or a nested replacement field.
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        spec.precision = parse_count(&chars, &mut i)?;
        if matches!(spec.precision, Count::None) {
            return Err("precision without digits".into());
        }
    }
    // type
    let rest: String = chars[i..].iter().collect();
    spec.type_char = match rest.len() {
        0 => None,
        1 => {
            let c = rest.chars().next().unwrap();
            if !is_supported_type(c) {
                return Err(format!("unsupported type '{c}'"));
            }
            Some(c)
        }
        _ => return Err(format!("invalid type '{rest}'")),
    };
    Ok(spec)
}

/// Parse a width/precision component: either `{` [digits] `}` (dynamic ref) or
/// a run of decimal digits (literal). Returns [`Count::None`] if neither is
/// present at the current position.
fn parse_count(chars: &[char], i: &mut usize) -> Result<Count, String> {
    if *i < chars.len() && chars[*i] == '{' {
        *i += 1;
        let mut id_str = String::new();
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            id_str.push(chars[*i]);
            *i += 1;
        }
        if *i >= chars.len() || chars[*i] != '}' {
            return Err("unterminated nested width/precision ref".into());
        }
        *i += 1;
        if id_str.is_empty() {
            Ok(Count::Auto)
        } else {
            Ok(Count::Explicit(
                id_str
                    .parse::<usize>()
                    .map_err(|_| "bad nested arg id".to_string())?,
            ))
        }
    } else {
        let mut w = String::new();
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            w.push(chars[*i]);
            *i += 1;
        }
        if w.is_empty() {
            Ok(Count::None)
        } else {
            Ok(Count::Literal(
                w.parse::<usize>()
                    .map_err(|_| "bad width/precision".to_string())?,
            ))
        }
    }
}

/// Check if a character is a C++23 alignment direction (`<`, `>`, or `^`).
fn is_align(c: char) -> bool {
    matches!(c, '<' | '>' | '^')
}

/// C++23 type characters we implement. `a`/`A` (hex float) and `L` (locale) are
/// intentionally absent; any other char also yields an error.
fn is_supported_type(c: char) -> bool {
    matches!(
        c,
        'b' | 'B' | 'c' | 'd' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'o' | 'p' | 's' | 'x' | 'X'
    )
}

/// The rendered core of one argument, before width/fill/align padding.
struct Core {
    /// Whether the value is numeric (enables right-alignment by default and `0` padding).
    is_numeric: bool,
    /// True for integer/pointer renders (enables integer precision semantics
    /// and suppresses the `0` flag when precision is set).
    is_integer: bool,
    /// `""`, `"+"`, `" "`, or `"-"` — prepended outside the prefix/digits.
    sign: String,
    /// `"0x"`, `"0X"`, `"0"`, `"0b"`, `"0B"`, or `""` — between sign and digits.
    prefix: String,
    /// The main content (digits, text, or char).
    main: String,
}

impl Core {
    /// Concatenate `sign`, `prefix`, and `main` into the final rendered string.
    fn assemble(&self) -> String {
        format!("{}{}{}", self.sign, self.prefix, self.main)
    }
}

/// Render one wire argument according to its parsed format [`Spec`], applying
/// width, precision, zero-padding (`0` flag), fill, and alignment.
fn render_field(
    arg: &Arg,
    spec: &Spec,
    width: Option<usize>,
    precision: Option<usize>,
) -> Result<String, String> {
    let core = render_core(arg, spec, precision)?;
    // C++: the `0` flag is ignored when an align is present (any type), and
    // when a precision is specified for integer types.
    let zero_pad = spec.zero
        && spec.align.is_none()
        && core.is_numeric
        && !(core.is_integer && precision.is_some());
    Ok(apply_padding(core, spec, width, zero_pad))
}

/// Render the core value (before padding) by dispatching to the appropriate
/// type-specific renderer based on the spec type character and wire argument.
fn render_core(arg: &Arg, spec: &Spec, precision: Option<usize>) -> Result<Core, String> {
    let tc = spec.type_char;
    match arg {
        Arg::Str(opt) => render_str(opt, tc, precision),
        Arg::Ptr(v) => {
            // C++ default for a pointer argument is the address form.
            let effective = if tc.is_none() { Some('p') } else { tc };
            render_int(*v as u64, effective, spec, precision, true)
        }
        Arg::U32(v) => render_int(*v as u64, tc, spec, precision, false),
        Arg::U64(v) => {
            if matches!(tc, Some('f' | 'F' | 'e' | 'E' | 'g' | 'G')) {
                render_float(f64::from_bits(*v), tc, precision, spec.sign)
            } else {
                render_int(*v, tc, spec, precision, false)
            }
        }
    }
}

/// Render a string argument, applying optional truncation precision.
/// Accepts `s` or no type character; returns an error for numeric/float types.
fn render_str(
    opt: &Option<Vec<u8>>,
    tc: Option<char>,
    precision: Option<usize>,
) -> Result<Core, String> {
    if !matches!(tc, None | Some('s')) {
        return Err(format!("cannot format string as {:?}", tc));
    }
    let text = match opt {
        None => "(null)".to_string(),
        Some(b) => String::from_utf8_lossy(b).into_owned(),
    };
    let main = match precision {
        Some(p) => truncate_chars(&text, p),
        None => text,
    };
    Ok(Core {
        is_numeric: false,
        is_integer: false,
        sign: String::new(),
        prefix: String::new(),
        main,
    })
}

/// Render an integer argument according to the type character and format spec.
///
/// Handles decimal (`d` or default), hex (`x`/`X`), octal (`o`), binary (`b`/`B`),
/// pointer (`p`), char (`c`), and sign/alternate-form/prefix rendering.
/// Returns an error for string or float type characters on integer arguments.
fn render_int(
    v: u64,
    tc: Option<char>,
    spec: &Spec,
    precision: Option<usize>,
    is_ptr_arg: bool,
) -> Result<Core, String> {
    // Char-from-codepoint: not numeric for padding/sign purposes.
    if tc == Some('c') {
        let ch = char::from_u32(v as u32).unwrap_or('\u{FFFD}');
        return Ok(Core {
            is_numeric: false,
            is_integer: false,
            sign: String::new(),
            prefix: String::new(),
            main: ch.to_string(),
        });
    }

    let (digits, prefix): (String, &str) = match tc {
        None | Some('d') => (v.to_string(), ""),
        Some('x') => (format!("{:x}", v), if spec.alt { "0x" } else { "" }),
        Some('X') => (format!("{:X}", v), if spec.alt { "0X" } else { "" }),
        Some('o') => (format!("{:o}", v), if spec.alt { "0" } else { "" }),
        Some('b') => (format!("{:b}", v), if spec.alt { "0b" } else { "" }),
        Some('B') => (format!("{:b}", v), if spec.alt { "0B" } else { "" }),
        Some('p') => (format!("{:x}", v), "0x"),
        Some('s') => return Err("cannot format integer as string".into()),
        Some('f' | 'F' | 'e' | 'E' | 'g' | 'G') if !is_ptr_arg => {
            return Err("cannot format 32-bit integer as float".into());
        }
        Some(other) => return Err(format!("unsupported type '{other}'")),
    };

    // Integer precision = minimum number of digits (zero-padded after prefix).
    // C++: precision 0 with value 0 yields an empty digit string.
    let main = apply_int_precision(&digits, v, precision);

    // Sign does not apply to pointer rendering.
    let sign = if tc != Some('p') {
        match spec.sign {
            Some('+') => "+".to_string(),
            Some(' ') => " ".to_string(),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    Ok(Core {
        is_numeric: true,
        is_integer: true,
        sign,
        prefix: prefix.to_string(),
        main,
    })
}

/// Apply integer precision (minimum number of digits, zero-padded on the left).
/// C++23: precision 0 with value 0 yields an empty string.
fn apply_int_precision(digits: &str, v: u64, precision: Option<usize>) -> String {
    match precision {
        Some(p) => {
            if p == 0 && v == 0 {
                return String::new();
            }
            let len = digits.chars().count();
            if p > len {
                let zeros = "0".repeat(p - len);
                format!("{zeros}{digits}")
            } else {
                digits.to_string()
            }
        }
        None => digits.to_string(),
    }
}

/// Render an `f64` argument using C++23 float type characters (`f`/`F`/`e`/`E`/`g`/`G`).
///
/// `std::fmt` does the digit conversion; `inf`/`nan` casing is normalised
/// to match C++ conventions (lowercase for lower-case types, uppercase for
/// upper-case types).
fn render_float(
    x: f64,
    tc: Option<char>,
    precision: Option<usize>,
    sign_flag: Option<char>,
) -> Result<Core, String> {
    let tc = match tc {
        Some(c @ ('f' | 'F' | 'e' | 'E' | 'g' | 'G')) => c,
        _ => return Err("not a float type".into()),
    };
    let prec = precision.unwrap_or(6);
    let negative = x.is_sign_negative() && !x.is_nan();
    let xa = x.abs();

    // `std::fmt` does the conversion; we normalize inf/nan casing to match C++
    // (`inf`/`nan` lowercase, `INF`/`NAN` uppercase).
    let main = match tc {
        'f' => format!("{:.prec$}", xa, prec = prec).replace("NaN", "nan"),
        'F' => format!("{:.prec$}", xa, prec = prec).to_uppercase(),
        'e' => format!("{:.prec$e}", xa, prec = prec).replace("NaN", "nan"),
        'E' => format!("{:.prec$E}", xa, prec = prec).to_uppercase(),
        'g' => format!("{:?}", xa).replace("NaN", "nan"),
        'G' => format!("{:?}", xa).to_uppercase(),
        _ => unreachable!(),
    };

    let sign = if negative {
        "-".to_string()
    } else {
        match sign_flag {
            Some('+') => "+".to_string(),
            Some(' ') => " ".to_string(),
            _ => String::new(),
        }
    };

    Ok(Core {
        is_numeric: true,
        is_integer: false,
        sign,
        prefix: String::new(),
        main,
    })
}

/// Apply width / fill / align / `0` zero-pad around a rendered core value.
///
/// When `zero_pad` is true, zeros are inserted between the sign/prefix and the
/// main digits. Otherwise, fill characters (defaulting to space) pad according
/// to the alignment direction (left, right, or center). Default alignment for
/// numeric types is right, for non-numeric types is left.
fn apply_padding(core: Core, spec: &Spec, width: Option<usize>, zero_pad: bool) -> String {
    let content = core.assemble();
    let width = width.unwrap_or(0);
    let content_len = content.chars().count();
    if width == 0 || content_len >= width {
        return content;
    }
    let pad_count = width - content_len;

    if zero_pad {
        let mut s = String::with_capacity(width);
        s.push_str(&core.sign);
        s.push_str(&core.prefix);
        for _ in 0..pad_count {
            s.push('0');
        }
        s.push_str(&core.main);
        return s;
    }

    let (fill, align) = match spec.align {
        Some(a) => (spec.fill.unwrap_or(' '), a),
        None => {
            let a = if core.is_numeric {
                Align::Right
            } else {
                Align::Left
            };
            (' ', a)
        }
    };

    match align {
        Align::Left => {
            let mut s = content;
            for _ in 0..pad_count {
                s.push(fill);
            }
            s
        }
        Align::Right => {
            let mut s = String::with_capacity(width);
            for _ in 0..pad_count {
                s.push(fill);
            }
            s.push_str(&content);
            s
        }
        Align::Center => {
            let left = pad_count / 2;
            let right = pad_count - left;
            let mut s = String::with_capacity(width);
            for _ in 0..left {
                s.push(fill);
            }
            s.push_str(&content);
            for _ in 0..right {
                s.push(fill);
            }
            s
        }
    }
}

/// Truncate `s` to at most `max` Unicode scalar values (not bytes).
///
/// Used to implement string precision: `{:.5s}` limits the output to 5
/// Unicode code points.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::printf::render_format;

    fn s(b: &str) -> Arg {
        Arg::Str(Some(b.as_bytes().to_vec()))
    }

    fn f(x: f64) -> Arg {
        Arg::U64(x.to_bits())
    }

    #[test]
    fn defaults_by_type() {
        assert_eq!(render("n={}", &[Arg::U32(42)]), "n=42");
        assert_eq!(
            render("n={}", &[Arg::U64(u64::MAX)]),
            "n=18446744073709551615"
        );
        assert_eq!(render("p={}", &[Arg::Ptr(0x4002_1234)]), "p=0x40021234");
        assert_eq!(render("x={}", &[s("hi")]), "x=hi");
        assert_eq!(render("x={}", &[Arg::Str(None)]), "x=(null)");
    }

    #[test]
    fn d_and_s_explicit() {
        assert_eq!(render("{:d}", &[Arg::U32(255)]), "255");
        assert_eq!(render("{:s}", &[s("foo")]), "foo");
        assert_eq!(render("c={:c}", &[Arg::U32(65)]), "c=A");
        assert_eq!(render("{:c}", &[Arg::U32(0x1f600)]), "😀");
    }

    #[test]
    fn hex_oct_bin_and_alt() {
        assert_eq!(render("{:x}", &[Arg::U32(255)]), "ff");
        assert_eq!(render("{:X}", &[Arg::U32(255)]), "FF");
        assert_eq!(render("{:o}", &[Arg::U32(8)]), "10");
        assert_eq!(render("{:b}", &[Arg::U32(5)]), "101");
        assert_eq!(render("{:B}", &[Arg::U32(5)]), "101");
        assert_eq!(render("{:#x}", &[Arg::U32(255)]), "0xff");
        assert_eq!(render("{:#o}", &[Arg::U32(8)]), "010");
        assert_eq!(render("{:#b}", &[Arg::U32(5)]), "0b101");
    }

    #[test]
    fn pointer_explicit() {
        let r = render("at {:p}", &[Arg::Ptr(0x4002_1234)]);
        assert_eq!(r, "at 0x40021234");
    }

    #[test]
    fn width_align_fill() {
        assert_eq!(render("[{:>10}]", &[Arg::U32(5)]), "[         5]");
        assert_eq!(render("[{:<10}]", &[s("hi")]), "[hi        ]");
        assert_eq!(render("[{:^10}]", &[Arg::U32(5)]), "[    5     ]");
        assert_eq!(render("[{:*>10}]", &[Arg::U32(5)]), "[*********5]");
        assert_eq!(render("[{:*<10}]", &[s("hi")]), "[hi********]");
    }

    #[test]
    fn zero_pad_and_sign() {
        assert_eq!(render("{:08x}", &[Arg::U32(255)]), "000000ff");
        assert_eq!(render("{:#08x}", &[Arg::U32(255)]), "0x0000ff");
        assert_eq!(render("{:+d}", &[Arg::U32(5)]), "+5");
        assert_eq!(render("{: d}", &[Arg::U32(5)]), " 5");
        assert_eq!(render("{:-d}", &[Arg::U32(5)]), "5");
        // zero ignored when align present
        assert_eq!(render("[{:>08}]", &[Arg::U32(5)]), "[       5]");
    }

    #[test]
    fn string_precision() {
        assert_eq!(render("{:.3s}", &[s("foobar")]), "foo");
        assert_eq!(render("{:.0s}", &[s("foobar")]), "");
        assert_eq!(render("{:.100s}", &[s("hi")]), "hi");
    }

    #[test]
    fn integer_precision() {
        // min-digits zero pad
        assert_eq!(render("{:.4d}", &[Arg::U32(5)]), "0005");
        assert_eq!(render("{:.4x}", &[Arg::U32(0xff)]), "00ff");
        // prefix goes before precision zeros
        assert_eq!(render("{:#.4x}", &[Arg::U32(0xff)]), "0x00ff");
        assert_eq!(render("{:+.4d}", &[Arg::U32(5)]), "+0005");
        // precision 0 with value 0 -> empty
        assert_eq!(render("[{:.0}]", &[Arg::U32(0)]), "[]");
        // precision 0 with non-zero -> digits unchanged
        assert_eq!(render("[{:.0}]", &[Arg::U32(7)]), "[7]");
        // `0` flag ignored when precision is set for integers
        assert_eq!(render("[{:08.4d}]", &[Arg::U32(5)]), "[    0005]");
    }

    #[test]
    fn floats() {
        let pi = std::f64::consts::PI.to_bits();
        assert_eq!(render("{:.2f}", &[Arg::U64(pi)]), "3.14");
        assert_eq!(render("{:.2F}", &[Arg::U64(pi)]), "3.14");
        // C++ `{:e}` default precision is 6. Rust's LowerExp uses a 1-digit
        // exponent (`e0`) instead of C++'s `e+00`; that's a known difference.
        assert_eq!(render("{:e}", &[f(1.0)]), "1.000000e0");
        assert_eq!(render("{:E}", &[f(1.0)]), "1.000000E0");
        assert_eq!(render("{:.2e}", &[f(1.0)]), "1.00e0");
        // default precision for f is 6
        assert_eq!(render("{:f}", &[f(1.0)]), "1.000000");
        // negative
        assert_eq!(render("{:.1f}", &[f(-3.5)]), "-3.5");
        // `+`/` ` sign flags on non-negative floats
        assert_eq!(render("{:+.1f}", &[f(3.5)]), "+3.5");
        assert_eq!(render("{: .1f}", &[f(3.5)]), " 3.5");
        assert_eq!(render("{:+.1f}", &[f(-3.5)]), "-3.5");
        // inf / nan casing
        assert_eq!(render("{:f}", &[f(f64::INFINITY)]), "inf");
        assert_eq!(render("{:F}", &[f(f64::INFINITY)]), "INF");
        assert_eq!(render("{:f}", &[f(f64::NAN)]), "nan");
        assert_eq!(render("{:F}", &[f(f64::NAN)]), "NAN");
    }

    #[test]
    fn dynamic_width_auto() {
        // value=arg0, width=arg1
        assert_eq!(render("[{:{}}]", &[Arg::U32(42), Arg::U32(5)]), "[   42]");
        assert_eq!(render("[{:<{}}]", &[s("hi"), Arg::U32(5)]), "[hi   ]");
    }

    #[test]
    fn dynamic_precision_auto() {
        // value=arg0, precision=arg1. The float case needs an explicit `:f`
        // type char — plain `{}` on a U64 renders the raw bits as an integer
        // (the wire carries no int/float distinction; see known limitations).
        assert_eq!(
            render("{:.{}f}", &[f(std::f64::consts::PI), Arg::U32(2)]),
            "3.14"
        );
        assert_eq!(render("{:.{}s}", &[s("foobar"), Arg::U32(3)]), "foo");
        // value=arg0 (float via :f), width=arg1, precision=arg2
        assert_eq!(
            render(
                "[{:{}.{}f}]",
                &[f(std::f64::consts::PI), Arg::U32(8), Arg::U32(2)]
            ),
            "[    3.14]"
        );
        // value=arg0 (int), width=arg1, precision=arg2 (min digits)
        assert_eq!(
            render("[{:{}.{}}]", &[Arg::U32(42), Arg::U32(8), Arg::U32(4)]),
            "[    0042]"
        );
    }

    #[test]
    fn dynamic_width_precision_explicit() {
        assert_eq!(render("{0:{1}}", &[Arg::U32(42), Arg::U32(5)]), "   42");
        assert_eq!(
            render("{0:.{1}f}", &[f(std::f64::consts::PI), Arg::U32(2)]),
            "3.14"
        );
        // reuse: value and width both reference arg 0
        assert_eq!(render("{0:{0}}", &[Arg::U32(5)]), "    5");
    }

    #[test]
    fn dynamic_count_negative_is_error() {
        let r = render("[{:{}}]", &[Arg::U32(1), Arg::U32(0xffff_ffff)]);
        assert!(r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn dynamic_count_string_is_error() {
        let r = render("[{:{}}]", &[Arg::U32(1), s("x")]);
        assert!(r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn dynamic_count_missing_is_error() {
        let r = render("[{:{}}]", &[Arg::U32(1)]);
        assert!(r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn positional_and_auto() {
        assert_eq!(render("{0}-{1}-{0}", &[Arg::U32(7), Arg::U32(8)]), "7-8-7");
        assert_eq!(render("{}-{}", &[Arg::U32(7), Arg::U32(8)]), "7-8");
    }

    #[test]
    fn brace_escapes() {
        assert_eq!(render("{{}} and {}", &[Arg::U32(1)]), "{} and 1");
        assert_eq!(render("{{literal}}", &[]), "{literal}");
    }

    #[test]
    fn mixing_auto_and_explicit_is_error() {
        let r = render("{} {0}", &[Arg::U32(1), Arg::U32(2)]);
        assert!(r.starts_with("<render error"), "got {r}");
        // mixed via nested ref
        let r = render("{:{0}}", &[Arg::U32(1), Arg::U32(5)]);
        assert!(r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn missing_arg_is_error() {
        let r = render("{} {}", &[Arg::U32(1)]);
        assert!(r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn unsupported_types_are_errors() {
        // Excluded: L (locale), a/A (hex float)
        let r = render("{:Ld}", &[Arg::U32(1)]);
        assert!(r.starts_with("<render error"), "got {r}");
        let r = render("{:a}", &[Arg::U64(0)]);
        assert!(r.starts_with("<render error"), "got {r}");
        let r = render("{:A}", &[Arg::U64(0)]);
        assert!(r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn type_mismatch_is_error() {
        let r = render("{:d}", &[s("foo")]);
        assert!(r.starts_with("<render error"), "got {r}");
        let r = render("{:s}", &[Arg::U32(1)]);
        assert!(r.starts_with("<render error"), "got {r}");
        let r = render("{:f}", &[Arg::U32(1)]);
        assert!(r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn detection() {
        assert!(looks_like_cpp("count={:d}"));
        assert!(looks_like_cpp("n={}"));
        assert!(looks_like_cpp("{0:>10.2f}"));
        assert!(!looks_like_cpp("count=%d"));
        // literal braces that are not field-ish stay printf
        assert!(!looks_like_cpp("json {key=%d}"));
        // escaped braces are not fields
        assert!(!looks_like_cpp("{{not a field}}"));
    }

    #[test]
    fn complex_combined() {
        let r = render(
            "[{:>8.2f}] {:s}={:#06x}",
            &[f(std::f64::consts::PI), s("id"), Arg::U32(0xab)],
        );
        assert_eq!(r, "[    3.14] id=0x00ab");
    }

    // ---- Dispatcher safety (finding 1) ----

    #[test]
    fn dispatcher_prefers_printf_when_percent_conversion_present() {
        // Mixed string: has both {} and %d. Must route to printf so `status`
        // is consumed by %d and {} stays literal — NOT to cppfmt where {}
        // would steal status.
        let r = render_format("payload={} status=%d", &[Arg::U32(7)]);
        assert!(r.contains("status=7"), "got {r}");
        assert!(r.contains("payload={}"), "got {r}");
        assert!(!r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn dispatcher_prefers_printf_for_dynamic_width_precision() {
        let r = render_format("payload={} value=%*d", &[Arg::U32(5), Arg::U32(7)]);
        assert!(r.contains("payload={}"), "got {r}");
        assert!(r.contains("value=    7"), "got {r}");
        assert!(!r.starts_with("<render error"), "got {r}");

        let r = render_format("payload={} text=%.*s", &[Arg::U32(3), s("foobar")]);
        assert!(r.contains("payload={}"), "got {r}");
        assert!(r.contains("text=foo"), "got {r}");
        assert!(!r.starts_with("<render error"), "got {r}");
    }

    #[test]
    fn dispatcher_routes_pure_cpp() {
        assert_eq!(render_format("n={:d}", &[Arg::U32(7)]), "n=7");
        assert_eq!(render_format("n={}", &[Arg::U32(7)]), "n=7");
    }

    #[test]
    fn dispatcher_routes_pure_printf() {
        assert_eq!(render_format("n=%d", &[Arg::U32(7)]), "n=7");
    }

    #[test]
    fn dispatcher_treats_literal_percent_in_cpp_as_cpp() {
        // A literal `%` that is NOT followed by a conversion-looking sequence
        // (here `% ` then `|`) leaves has_printf_conversion false, so the C++
        // field routes to cppfmt. NB: a `%` directly followed by a letter such
        // as `%d` or even `% d` (space flag + d) IS a printf conversion and
        // wins the dispatch — that is intentional and matches the firmware's
        // format(printf) annotation.
        let r = render_format("progress: 100% | val={:d}", &[Arg::U32(7)]);
        assert_eq!(r, "progress: 100% | val=7");
    }

    #[test]
    fn dispatcher_single_percent_with_field_routes_to_cpp() {
        // Single `%` (no `%%` printf escape needed in C++ strings) followed by
        // a space and a non-conversion char -> no printf conversion -> cppfmt.
        let r = render_format("load=50% val={}", &[Arg::U32(7)]);
        assert_eq!(r, "load=50% val=7");
    }
}
