//! printf format-string rendering, delegated to the `sprintf` crate.
//!
//! The firmware ships only the format string address; the host must consume
//! decoded arguments in order. We hand rendering to `sprintf::vsprintf`, which
//! does all digit conversion, padding, hex/float formatting, etc.
//!
//! Two things force a small amount of glue around `vsprintf`:
//!
//! 1. `sprintf` dispatches by the *Rust type* of each argument (via downcast)
//!    and is strict — `%d` wants a signed int, `%u`/`%x` an unsigned int, `%c`
//!    a `u32`, `%f` an `f64`, `%p` a raw pointer, `%s` a string. The on9log wire
//!    only carries raw 32/64-bit values without signedness, so we scan the
//!    format string to recover each conversion character and use it to coerce
//!    each wire argument to the matching Rust type. `sprintf`'s own parser
//!    collapses `d`/`i`/`u` into one variant, so it cannot make this call.
//! 2. `sprintf` 0.4.3 mishandles dynamic precision (`%.*s`): the `*` argument
//!    is rejected for every integer type. We work around this by resolving
//!    `*` width/precision arguments to literal values ourselves while scanning,
//!    so `vsprintf` only ever sees value arguments.
//!
//! The scanner does no formatting itself — `sprintf` renders everything.

use sprintf::{Printf, vsprintf};

use crate::MAX_RENDER_COUNT;

/// Dispatch `fmt` to the right renderer.
///
/// A format string is routed to the C++23 ([`crate::cppfmt`]) renderer only if
/// it contains a C++23-style replacement field *and* no active printf
/// conversion. Mixed strings such as `"payload={} status=%d"` stay on the
/// printf path so the `%d` consumes its argument and `{}` is treated as a
/// literal — this matches the firmware macro's `format(printf, 3, 5)`
/// annotation, which is how the compiler interprets the string. Pure `{}`-style
/// strings (no `%` conversions) route to the C++ renderer.
pub fn render_format(fmt: &str, args: &[Arg]) -> String {
    if crate::cppfmt::looks_like_cpp(fmt) && !has_printf_conversion(fmt) {
        crate::cppfmt::render(fmt, args)
    } else {
        render(fmt, args)
    }
}

/// True if `fmt` contains at least one active printf conversion — a `%` (not
/// `%%`) followed by optional flags/width/precision/length modifiers and then a
/// conversion character in `[diouxXeEfgGcspn]`. Used by [`render_format`] to
/// keep mixed strings on the printf path.
pub fn has_printf_conversion(fmt: &str) -> bool {
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '%' {
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            break;
        }
        if chars[i] == '%' {
            i += 1;
            continue;
        }
        // flags
        while i < chars.len() && matches!(chars[i], '-' | '+' | ' ' | '#' | '0') {
            i += 1;
        }
        // width
        if i < chars.len() && chars[i] == '*' {
            i += 1;
        } else {
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
        }
        // precision
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            if i < chars.len() && chars[i] == '*' {
                i += 1;
            } else {
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
        }
        // length modifiers (handles hh/ll by consuming runs)
        while i < chars.len() && matches!(chars[i], 'h' | 'l' | 'j' | 'z' | 't' | 'L') {
            i += 1;
        }
        // conversion character
        if i < chars.len()
            && matches!(
                chars[i],
                'd' | 'i'
                    | 'o'
                    | 'u'
                    | 'x'
                    | 'X'
                    | 'e'
                    | 'E'
                    | 'f'
                    | 'F'
                    | 'g'
                    | 'G'
                    | 'c'
                    | 's'
                    | 'p'
                    | 'n'
            )
        {
            return true;
        }
        // not a valid conversion; keep scanning from here
    }
    false
}

/// One decoded argument value, already typed via the payload's arg-type table.
#[derive(Debug, Clone)]
pub enum Arg {
    /// A 32-bit unsigned value (maps to `ArgType::Bits32`).
    U32(u32),
    /// A 64-bit unsigned value (maps to `ArgType::Bits64`, also carries `f64` bit patterns).
    U64(u64),
    /// A 32-bit pointer value (maps to `ArgType::Pointer`).
    Ptr(u32),
    /// A dynamic string (maps to `ArgType::DynamicString`).
    /// `None` represents a null dynamic string (length 0xffffffff).
    Str(Option<Vec<u8>>),
}

/// Owned backing storage for a value that implements `Printf`. We need a
/// concrete place for each `&dyn Printf` to borrow from, and the values are
/// heterogeneous, so a small enum holds them.
enum Owned {
    /// Signed 8-bit integer (from `%hhd` conversion).
    I8(i8),
    /// Unsigned 8-bit integer (from `%hhu` conversion).
    U8(u8),
    /// Signed 16-bit integer (from `%hd` conversion).
    I16(i16),
    /// Unsigned 16-bit integer (from `%hu` conversion).
    U16(u16),
    /// Signed 32-bit integer (from `%d`, `%ld`, `%zd`, etc. on ESP32).
    I32(i32),
    /// Unsigned 32-bit integer (from `%u`, `%x`, `%lx`, etc. on ESP32).
    U32(u32),
    /// Signed 64-bit integer (from `%lld` conversion).
    I64(i64),
    /// Unsigned 64-bit integer (from `%llu`, `%llx`, etc.).
    U64(u64),
    /// 64-bit float interpreted from `Arg::U64` bit pattern.
    F64(f64),
    /// String storage (owned heap-allocated).
    Str(String),
    /// Raw pointer (from `%p` conversion).
    Ptr(*const u8),
}

impl Owned {
    /// Return a `&dyn Printf` reference suitable for `sprintf::vsprintf`.
    fn as_printf(&self) -> &dyn Printf {
        match self {
            Owned::I8(v) => v,
            Owned::U8(v) => v,
            Owned::I16(v) => v,
            Owned::U16(v) => v,
            Owned::I32(v) => v,
            Owned::U32(v) => v,
            Owned::I64(v) => v,
            Owned::U64(v) => v,
            Owned::F64(v) => v,
            Owned::Str(v) => v,
            Owned::Ptr(v) => v,
        }
    }
}

/// Render `fmt` consuming `args` left-to-right via `sprintf::vsprintf`.
///
/// On a formatting error (unsupported conversion, type mismatch, missing
/// argument) the format string itself is returned with a marker so the log
/// stream is not lost.
pub fn render(fmt: &str, args: &[Arg]) -> String {
    let mut new_fmt = String::with_capacity(fmt.len());
    let mut owned: Vec<Owned> = Vec::with_capacity(args.len());
    let mut ai = 0usize;
    let mut chars = fmt.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '%' {
            new_fmt.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            new_fmt.push_str("%%");
            chars.next();
            continue;
        }
        new_fmt.push('%');

        // Flags — copied through.
        let mut has_minus_flag = false;
        while matches!(chars.peek(), Some('-' | '+' | ' ' | '#' | '0')) {
            let flag = chars.next().unwrap();
            if flag == '-' {
                has_minus_flag = true;
            }
            new_fmt.push(flag);
        }

        // Width — `*` is resolved to a literal so we control arg typing.
        match chars.peek().copied() {
            Some('*') => {
                chars.next();
                let v = next_int(args, &mut ai);
                push_width_literal(&mut new_fmt, v, &mut has_minus_flag);
            }
            Some(d) if d.is_ascii_digit() => {
                while matches!(chars.peek(), Some(d) if d.is_ascii_digit()) {
                    new_fmt.push(chars.next().unwrap());
                }
            }
            _ => {}
        }

        // Precision — `.*` is resolved to a literal (sprintf 0.4.3 bug workaround).
        if chars.peek() == Some(&'.') {
            chars.next();
            match chars.peek().copied() {
                Some('*') => {
                    chars.next();
                    let v = next_int(args, &mut ai);
                    push_precision_literal(&mut new_fmt, v);
                }
                Some(d) if d.is_ascii_digit() => {
                    new_fmt.push('.');
                    while matches!(chars.peek(), Some(d) if d.is_ascii_digit()) {
                        new_fmt.push(chars.next().unwrap());
                    }
                }
                _ => {
                    // `%.` with no digits → precision 0, keep as-is.
                    new_fmt.push('.');
                }
            }
        }

        // Length modifiers are copied for parser compatibility, but we also
        // use them to pick the Rust integer width passed to `sprintf`.
        let len = parse_length(&mut chars, &mut new_fmt);

        // Conversion character — consumes the value argument.
        if let Some(conv) = chars.next() {
            new_fmt.push(conv);
            if ai < args.len() {
                let arg = &args[ai];
                ai += 1;
                owned.push(value_to_owned(len, conv, arg));
            }
        }
    }

    let refs: Vec<&dyn Printf> = owned.iter().map(Owned::as_printf).collect();
    match vsprintf(&new_fmt, &refs) {
        Ok(s) => s,
        Err(e) => format!("<render error: {e}> {fmt}"),
    }
}

/// Read the next wire arg's integer value (for `*` params), advancing the index.
///
/// Returns `0` if no arguments remain.
fn next_int(args: &[Arg], ai: &mut usize) -> i64 {
    if *ai < args.len() {
        let v = match &args[*ai] {
            Arg::U32(v) => *v as i32 as i64,
            Arg::U64(v) => *v as i64,
            Arg::Ptr(v) => *v as i32 as i64,
            Arg::Str(_) => 0,
        };
        *ai += 1;
        v
    } else {
        0
    }
}

/// Emit a resolved `*` width into the format. C semantics: a negative width
/// means left-justify with width `abs(v)`; zero means no padding.
fn push_width_literal(out: &mut String, v: i64, has_minus_flag: &mut bool) {
    let width = v.unsigned_abs().min(MAX_RENDER_COUNT as u64);
    if v < 0 {
        if !*has_minus_flag {
            out.push('-');
            *has_minus_flag = true;
        }
        out.push_str(&width.to_string());
    } else if v > 0 {
        out.push_str(&width.to_string());
    }
    // v == 0 → emit nothing (no padding).
}

/// Emit a resolved `.*` precision into the format. C semantics: a negative
/// precision is treated as if the precision were omitted.
fn push_precision_literal(out: &mut String, v: i64) {
    if v >= 0 {
        out.push('.');
        out.push_str(&(v as u64).min(MAX_RENDER_COUNT as u64).to_string());
    }
    // v < 0 → omit the precision entirely.
}

/// C printf length modifier, used to pick the Rust integer width for `sprintf`.
#[derive(Clone, Copy)]
enum Length {
    /// No length modifier (default int width).
    Default,
    /// `hh` — `signed char` / `unsigned char`.
    Char,
    /// `h` — `short` / `unsigned short`.
    Short,
    /// `l` — `long` / `unsigned long`.
    Long,
    /// `ll` — `long long` / `unsigned long long`.
    LongLong,
    /// `z` — `size_t` / `ssize_t`.
    Size,
    /// `j` — `intmax_t` / `uintmax_t`.
    IntMax,
    /// `t` — `ptrdiff_t`.
    PtrDiff,
    /// `L` — `long double` (treated as 32-bit on ESP32, same as default).
    LongDouble,
}

/// Consume a C printf length modifier from the character stream, push it onto
/// `out`, and return the corresponding [`Length`] variant.
fn parse_length<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
    out: &mut String,
) -> Length {
    match chars.peek().copied() {
        Some('h') => {
            chars.next();
            out.push('h');
            if chars.peek() == Some(&'h') {
                chars.next();
                out.push('h');
                Length::Char
            } else {
                Length::Short
            }
        }
        Some('l') => {
            chars.next();
            out.push('l');
            if chars.peek() == Some(&'l') {
                chars.next();
                out.push('l');
                Length::LongLong
            } else {
                Length::Long
            }
        }
        Some('z') => {
            chars.next();
            out.push('z');
            Length::Size
        }
        Some('j') => {
            chars.next();
            out.push('j');
            Length::IntMax
        }
        Some('t') => {
            chars.next();
            out.push('t');
            Length::PtrDiff
        }
        Some('L') => {
            chars.next();
            out.push('L');
            Length::LongDouble
        }
        _ => Length::Default,
    }
}

/// Coerce a wire `Arg` to the Rust type its conversion character expects.
///
/// The coercion is driven by the printf [`Length`] modifier and the conversion
/// character, so the value passed to `sprintf::vsprintf` has the correct Rust
/// type for `%d` (signed), `%u`/`%x` (unsigned), `%f` (f64), `%p` (pointer),
/// and `%s` (string).
fn value_to_owned(len: Length, conv: char, arg: &Arg) -> Owned {
    match arg {
        Arg::Str(Some(b)) => Owned::Str(String::from_utf8_lossy(b).into_owned()),
        Arg::Str(None) => Owned::Str("(null)".to_string()),
        Arg::Ptr(v) => match conv {
            'p' => Owned::Ptr(*v as *const u8),
            // Non-%p use of a pointer address: treat as a 32-bit value.
            'd' | 'i' => signed_int(*v as u64, len),
            _ => unsigned_int(*v as u64, len),
        },
        Arg::U32(v) => match conv {
            'd' | 'i' => signed_int(*v as u64, len),
            'p' => Owned::Ptr(*v as *const u8),
            _ => unsigned_int(*v as u64, len),
        },
        Arg::U64(v) => match conv {
            'd' | 'i' => signed_int(*v, len),
            'f' | 'F' | 'e' | 'E' | 'g' | 'G' => Owned::F64(f64::from_bits(*v)),
            'p' => Owned::Ptr(*v as usize as *const u8),
            _ => unsigned_int(*v, len),
        },
    }
}

/// Cast a raw `u64` value to the signed integer width indicated by `len`.
///
/// On ESP32 targets, `long`, `size_t`, and `ptrdiff_t` are 32 bits wide.
fn signed_int(v: u64, len: Length) -> Owned {
    match len {
        Length::Char => Owned::I8(v as u8 as i8),
        Length::Short => Owned::I16(v as u16 as i16),
        Length::LongLong | Length::IntMax => Owned::I64(v as i64),
        // ESP32 targets are 32-bit; long, size_t and ptrdiff_t are 32-bit.
        Length::Default | Length::Long | Length::Size | Length::PtrDiff | Length::LongDouble => {
            Owned::I32(v as u32 as i32)
        }
    }
}

/// Cast a raw `u64` value to the unsigned integer width indicated by `len`.
///
/// On ESP32 targets, `long`, `size_t`, and `ptrdiff_t` are 32 bits wide.
fn unsigned_int(v: u64, len: Length) -> Owned {
    match len {
        Length::Char => Owned::U8(v as u8),
        Length::Short => Owned::U16(v as u16),
        Length::LongLong | Length::IntMax => Owned::U64(v),
        // ESP32 targets are 32-bit; long, size_t and ptrdiff_t are 32-bit.
        Length::Default | Length::Long | Length::Size | Length::PtrDiff | Length::LongDouble => {
            Owned::U32(v as u32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_printf_conversion_detects_active_conversions() {
        assert!(has_printf_conversion("n=%d"));
        assert!(has_printf_conversion("v=0x%04x s=%s"));
        assert!(has_printf_conversion("%llu %c %p %f %%trailer"));
        // length modifiers and mixed flags
        assert!(has_printf_conversion("%hhd %ld %lf"));
        assert!(has_printf_conversion("%*d %.*s"));
        // escaped percent is not a conversion
        assert!(!has_printf_conversion("load=50%%"));
        // bare trailing percent (no conversion char after) is not a conversion
        assert!(!has_printf_conversion("100%"));
        // `% d` (space flag + d) IS a real conversion, so this is true
        assert!(has_printf_conversion("100% done"));
        // `%` followed by a non-conversion letter is not a conversion
        assert!(!has_printf_conversion("foo %bar"));
    }

    #[test]
    fn simple_int_and_string() {
        let s = render(
            "count=%d name=%s",
            &[Arg::U32(42), Arg::Str(Some(b"foo".to_vec()))],
        );
        assert_eq!(s, "count=42 name=foo");
    }

    #[test]
    fn hex_and_padding() {
        let s = render("v=0x%04x", &[Arg::U32(255)]);
        assert_eq!(s, "v=0x00ff");
    }

    #[test]
    fn precision_string() {
        // %.*s consumes a precision arg then the string.
        let s = render("s=%.*s", &[Arg::U32(3), Arg::Str(Some(b"hello".to_vec()))]);
        assert_eq!(s, "s=hel");
    }

    #[test]
    fn width_star_string() {
        // %*s consumes a width arg then the string.
        let s = render("[%*s]", &[Arg::U32(10), Arg::Str(Some(b"hi".to_vec()))]);
        assert_eq!(s, "[        hi]");
    }

    #[test]
    fn mixed_star_widths_and_precision_strings() {
        let s = render(
            "widths [%*s] [%-*s] [%.*s]",
            &[
                Arg::U32(10),
                Arg::Str(Some(b"right".to_vec())),
                Arg::U32((-8i32) as u32),
                Arg::Str(Some(b"left".to_vec())),
                Arg::U32(5),
                Arg::Str(Some(b"truncate-me".to_vec())),
            ],
        );
        assert_eq!(s, "widths [     right] [left    ] [trunc]");
    }

    #[test]
    fn null_string() {
        let s = render("s=%s", &[Arg::Str(None)]);
        assert_eq!(s, "s=(null)");
    }

    #[test]
    fn long_long_unsigned() {
        let s = render("n=%llu", &[Arg::U64(u64::MAX)]);
        assert_eq!(s, "n=18446744073709551615");
    }

    #[test]
    fn signed_vs_unsigned_32bit() {
        // Same 32-bit pattern, interpreted by the conversion specifier.
        let s_d = render("%d", &[Arg::U32(0xffff_ffff)]);
        let s_u = render("%u", &[Arg::U32(0xffff_ffff)]);
        let s_x = render("%x", &[Arg::U32(0xffff_ffff)]);
        assert_eq!(s_d, "-1");
        assert_eq!(s_u, "4294967295");
        assert_eq!(s_x, "ffffffff");
    }

    #[test]
    fn literal_percent() {
        let s = render("load=50%%", &[]);
        assert_eq!(s, "load=50%");
    }

    #[test]
    fn short_and_char_length_modifiers() {
        assert_eq!(render("%hhd", &[Arg::U32(0xff)]), "-1");
        assert_eq!(render("%hhu", &[Arg::U32(0xff)]), "255");
        assert_eq!(render("%hd", &[Arg::U32(0xffff)]), "-1");
        assert_eq!(render("%hu", &[Arg::U32(0xffff)]), "65535");
    }

    #[test]
    fn negative_star_width_with_minus_flag() {
        let s = render(
            "[%-*s]",
            &[Arg::U32((-5i32) as u32), Arg::Str(Some(b"x".to_vec()))],
        );
        assert_eq!(s, "[x    ]");
    }

    #[test]
    fn dynamic_width_is_bounded() {
        let s = render("[%*d]", &[Arg::U32(0x8000_0000), Arg::U32(7)]);
        assert_eq!(s.len(), MAX_RENDER_COUNT + 2);
        assert!(s.starts_with("[7"));
        assert!(s.ends_with(']'));
    }

    #[test]
    fn dynamic_precision_is_bounded() {
        let s = render(
            "[%.*f]",
            &[Arg::U64(i64::MAX as u64), Arg::U64(1.0f64.to_bits())],
        );
        assert_eq!(s.len(), MAX_RENDER_COUNT + 4);
        assert!(s.starts_with("[1."));
        assert!(s.ends_with(']'));
    }

    #[test]
    fn pointer() {
        let s = render("p=%p", &[Arg::Ptr(0x4002_1234)]);
        assert!(!s.contains("<render error"), "got {s}");
        assert!(s.contains("40021234"), "got {s}");
    }

    #[test]
    fn float_from_64bit_bits() {
        let pi = std::f64::consts::PI.to_bits();
        let s = render("pi=%.2f", &[Arg::U64(pi)]);
        assert_eq!(s, "pi=3.14");
    }
}
