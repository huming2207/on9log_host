//! Resolve ELF or Mach-O addresses back to their C strings and symbols.
//!
//! The firmware places format strings in `.noload_keep_in_elf.*` sections and
//! tags in normal read-only sections, then sends only the address. The host
//! opens the matching ELF and maps each address to the NUL-terminated string
//! stored in any section that carries file bytes at that virtual address.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use goblin::{
    Object,
    elf::{Elf, SectionHeader, sym::STT_FUNC},
    mach::{Mach, MachO, symbols::N_SECT},
    strtab::Strtab,
};

/// Address-indexed executable-image string table.
///
/// Loads an ELF binary and provides address-to-string resolution for format
/// strings, tags, and function symbols, plus optional DWARF source-location
/// lookups when loaded from a file path.
pub struct ElfStrings {
    /// Sections sorted by start address, each carrying its file bytes.
    sections: Vec<Section>,
    /// Function symbols sorted by start address.
    symbols: Vec<Symbol>,
    /// DWARF-backed source location resolver, when this ELF was loaded by path.
    lines: Option<addr2line::Loader>,
    /// Runtime image slide reduced to the 32-bit address width carried on the
    /// wire. Zero for ESP ELF images and non-PIE host executables.
    address_slide: AtomicU32,
}

/// An ELF section with its virtual-address range and file-backed data.
struct Section {
    /// Section name (e.g. `.rodata`, `.noload`).
    name: String,
    /// Virtual start address of the section.
    addr: u32,
    /// Exclusive end address (`addr + section_size`).
    end: u32,
    /// Raw file bytes for this section (for string lookups).
    data: Vec<u8>,
}

/// A resolved symbol result: the containing function name, its base address,
/// and the byte offset from that base to the queried address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSymbol<'a> {
    /// Function symbol name.
    pub name: &'a str,
    /// Base address of the function.
    pub address: u32,
    /// Byte offset from `address` to the queried instruction address.
    pub offset: u32,
}

/// A source file and optional line number resolved from an instruction address
/// via DWARF debug information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// Source file path (as recorded in DWARF).
    pub file: String,
    /// 1-based line number, or `None` if the debug info lacks line data.
    pub line: Option<u32>,
}

/// A function symbol parsed from the ELF symbol table.
struct Symbol {
    /// Mangled or unmangled function name.
    name: String,
    /// Virtual address of the function entry point.
    addr: u32,
    /// Size of the function in bytes (0 if unknown).
    size: u32,
}

impl ElfStrings {
    /// Parse an ELF or thin Mach-O file from raw bytes.
    ///
    /// Scans all section headers and symbol tables, sorting sections and symbols
    /// by address for binary search at lookup time. Does not load DWARF debug
    /// info; use [`ElfStrings::from_path`] for source-location resolution.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut sections = Vec::new();
        let mut symbols = Vec::new();
        match Object::parse(bytes).map_err(|e| format!("executable parse: {e}"))? {
            Object::Elf(elf) => {
                for sh in &elf.section_headers {
                    let name = elf.shdr_strtab.get_at(sh.sh_name).unwrap_or_default();
                    collect_section(bytes, sh, name, &mut sections);
                }
                collect_symbols(&elf, &mut symbols);
            }
            Object::Mach(Mach::Binary(mach)) => {
                collect_mach_sections(&mach, &mut sections)?;
                collect_mach_symbols(&mach, &mut symbols);
            }
            Object::Mach(Mach::Fat(_)) => {
                return Err("fat Mach-O images are not supported; use a thin executable".into());
            }
            _ => return Err("unsupported executable format; expected ELF or Mach-O".into()),
        }

        // Sort by start address; drop overlaps by preferring earlier entry.
        sections.sort_by_key(|s| s.addr);
        symbols.sort_by_key(|s| (s.addr, std::cmp::Reverse(s.size)));
        Ok(Self {
            sections,
            symbols,
            lines: None,
            address_slide: AtomicU32::new(0),
        })
    }

    /// Parse an ELF or Mach-O file from disk and enable source location lookups.
    ///
    /// Reads the file, calls [`ElfStrings::from_bytes`], and then loads the
    /// DWARF debug info from the same path for [`resolve_location`](Self::resolve_location).
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
        let mut elf = Self::from_bytes(&bytes)?;
        elf.lines = addr2line::Loader::new(path).ok();
        Ok(elf)
    }

    /// Read a NUL-terminated string starting at `addr`, if it falls inside a
    /// known section. Returns `None` for unmapped addresses or empty strings.
    ///
    /// Searches all loaded sections regardless of section type or name.
    /// If the same address maps to different strings in different sections,
    /// returns `None` (ambiguous).
    pub fn read_cstr(&self, addr: u32) -> Option<&str> {
        self.read_cstr_from(addr, |_| true)
    }

    /// Set the runtime image slide used to normalize host-process pointers
    /// before looking them up in an ELF or Mach-O file.
    pub fn set_address_slide(&self, slide: u32) {
        self.address_slide.store(slide, Ordering::Relaxed);
    }

    /// Read a format string. C macro logs place formats in ESP-IDF's ELF-only
    /// no-load section family. The C++ header wrapper accepts normal function
    /// string arguments, so those literals remain in ordinary string-bearing
    /// sections and are used as a fallback.
    ///
    /// First searches sections whose names contain `.noload`; if no match is
    /// found, falls back to any loaded section.
    pub fn read_format(&self, addr: u32) -> Option<&str> {
        self.read_cstr_from(addr, is_noload_section)
            .or_else(|| self.read_cstr_from(addr, |_| true))
    }

    /// Read a normal tag string. Tags are expected in ordinary string-bearing
    /// sections, not the no-load format-string section.
    ///
    /// Searches all sections whose names do NOT contain `.noload`.
    pub fn read_tag(&self, addr: u32) -> Option<&str> {
        self.read_cstr_from(addr, |name| !is_noload_section(name))
    }

    /// Resolve an instruction address to the nearest containing function
    /// symbol. If the symbol has no size, the next symbol address bounds it.
    ///
    /// Uses binary search over the sorted symbol list. Returns the containing
    /// function name, its base address, and the offset from that base to `addr`.
    pub fn resolve_symbol(&self, addr: u32) -> Option<ResolvedSymbol<'_>> {
        let addr = addr.wrapping_sub(self.address_slide.load(Ordering::Relaxed));
        let idx = self.symbols.partition_point(|s| s.addr <= addr);
        for i in (0..idx).rev() {
            let sym = &self.symbols[i];
            let end = if sym.size > 0 {
                sym.addr.saturating_add(sym.size)
            } else {
                self.symbols
                    .iter()
                    .skip(i + 1)
                    .find(|next| next.addr > sym.addr)
                    .map(|next| next.addr)
                    .unwrap_or(u32::MAX)
            };
            if addr < end {
                return Some(ResolvedSymbol {
                    name: &sym.name,
                    address: sym.addr,
                    offset: addr.saturating_sub(sym.addr),
                });
            }
        }
        None
    }

    /// Resolve an instruction address to a DWARF source file and line.
    ///
    /// Requires the ELF to have been loaded via [`ElfStrings::from_path`] (which
    /// enables DWARF lookups). Returns `None` when no DWARF info is available
    /// or the address is not mapped to a known source location.
    pub fn resolve_location(&self, addr: u32) -> Option<SourceLocation> {
        let addr = addr.wrapping_sub(self.address_slide.load(Ordering::Relaxed));
        let loc = self.lines.as_ref()?.find_location(u64::from(addr)).ok()??;
        let file = loc.file?;
        Some(SourceLocation {
            file: file.to_string(),
            line: loc.line,
        })
    }

    /// Internal helper: find a NUL-terminated string at `addr`, restricting
    /// the search to sections that pass the `section_matches` predicate.
    ///
    /// If multiple sections contain `addr` but disagree on the string value,
    /// returns `None` (ambiguous).
    fn read_cstr_from<P>(&self, addr: u32, section_matches: P) -> Option<&str>
    where
        P: Fn(&str) -> bool,
    {
        let addr = addr.wrapping_sub(self.address_slide.load(Ordering::Relaxed));
        let mut found: Option<&str> = None;
        for sec in self
            .sections
            .iter()
            .filter(|s| section_matches(&s.name) && s.contains(addr))
        {
            let s = sec.read_cstr(addr)?;
            match found {
                Some(prev) if prev != s => return None,
                Some(_) => {}
                None => found = Some(s),
            }
        }
        found
    }
}

impl Section {
    /// Check if `addr` falls within this section's virtual address range.
    fn contains(&self, addr: u32) -> bool {
        self.addr <= addr && addr < self.end
    }

    /// Read the NUL-terminated string starting at `addr` within this section's
    /// data. Returns `None` if `addr` is out of range or the string is empty.
    fn read_cstr(&self, addr: u32) -> Option<&str> {
        let off = usize::try_from(addr - self.addr).ok()?;
        let bytes = self.data.get(off..)?;
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        if end == 0 {
            return None;
        }
        std::str::from_utf8(&bytes[..end]).ok()
    }
}

/// Check whether a section name identifies it as an ESP-IDF no-load section
/// (`.noload*`), which carry format strings at VMA 0.
fn is_noload_section(name: &str) -> bool {
    name.contains(".noload")
}

/// Collect a single ELF section's data into `out` if it has non-zero size
/// and is addressable. NOBITS sections (`SHT_NOBITS`) are skipped. VMA-0
/// sections are only kept when their name indicates a no-load section.
fn collect_section(file: &[u8], sh: &SectionHeader, name: &str, out: &mut Vec<Section>) {
    // NOBITS sections occupy no file space and hold no strings. ESP-IDF's
    // no-load strings are PROGBITS at VMA 0, so keep VMA-0 sections only when
    // their output section name identifies them as no-load.
    if sh.sh_size == 0 || sh.sh_type == 8 {
        return;
    }
    if sh.sh_addr == 0 && !is_noload_section(name) {
        return;
    }
    let start = usize::try_from(sh.sh_offset).unwrap_or(usize::MAX);
    let size = usize::try_from(sh.sh_size).unwrap_or(usize::MAX);
    let end = start.saturating_add(size);
    if start >= file.len() || end > file.len() {
        return;
    }
    let data = file[start..end].to_vec();
    let addr = u32::try_from(sh.sh_addr).unwrap_or(0);
    let end_addr = addr.saturating_add(u32::try_from(size).unwrap_or(0));
    out.push(Section {
        name: name.to_string(),
        addr,
        end: end_addr,
        data,
    });
}

/// Collect file-backed sections from a thin Mach-O image. on9log's wire IDs
/// are 32-bit, so the section VM addresses are intentionally reduced to their
/// low 32 bits. Linux uses a non-PIE demo; macOS replay removes the captured
/// image slide before lookup.
fn collect_mach_sections(mach: &MachO<'_>, out: &mut Vec<Section>) -> Result<(), String> {
    for segment in &mach.segments {
        for item in segment {
            let (section, data) = item.map_err(|e| format!("Mach-O section parse: {e}"))?;
            if data.is_empty() {
                continue;
            }
            let section_name = section.name().unwrap_or_default();
            let segment_name = section.segname().unwrap_or_default();
            let name = format!("{segment_name},{section_name}");
            let addr = section.addr as u32;
            let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
            out.push(Section {
                name,
                addr,
                end: addr.saturating_add(size),
                data: data.to_vec(),
            });
        }
    }
    Ok(())
}

/// Collect defined Mach-O symbols. Mach-O nlist entries do not carry symbol
/// sizes, so the next symbol address provides the upper bound during lookup.
fn collect_mach_symbols(mach: &MachO<'_>, out: &mut Vec<Symbol>) {
    for (name, symbol) in mach.symbols().flatten() {
        if name.is_empty() || symbol.n_value == 0 || symbol.is_stab() || symbol.get_type() != N_SECT
        {
            continue;
        }
        let addr = symbol.n_value as u32;
        if out.iter().any(|s| s.addr == addr && s.name == name) {
            continue;
        }
        out.push(Symbol {
            name: name.to_string(),
            addr,
            size: 0,
        });
    }
}

/// Collect all `STT_FUNC` symbols from both the normal and dynamic symbol
/// tables, deduplicating by (address, name).
fn collect_symbols(elf: &Elf<'_>, out: &mut Vec<Symbol>) {
    collect_symbol_table(elf.syms.iter(), &elf.strtab, out);
    collect_symbol_table(elf.dynsyms.iter(), &elf.dynstrtab, out);
}

/// Collect function symbols from one symbol table iterator, filtering to
/// `STT_FUNC` entries with a non-zero value and a non-empty name.
fn collect_symbol_table(
    symbols: impl Iterator<Item = goblin::elf::Sym>,
    names: &Strtab<'_>,
    out: &mut Vec<Symbol>,
) {
    for sym in symbols {
        if sym.st_value == 0 || sym.st_type() != STT_FUNC {
            continue;
        }
        let Some(name) = names.get_at(sym.st_name).filter(|s| !s.is_empty()) else {
            continue;
        };
        let addr = match u32::try_from(sym.st_value) {
            Ok(addr) => addr,
            Err(_) => continue,
        };
        let size = u32::try_from(sym.st_size).unwrap_or(0);
        if out.iter().any(|s| s.addr == addr && s.name == name) {
            continue;
        }
        out.push(Symbol {
            name: name.to_string(),
            addr,
            size,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_error() {
        assert!(ElfStrings::from_bytes(&[]).is_err());
    }

    #[test]
    fn parses_the_native_test_executable() {
        let path = std::env::current_exe().unwrap();
        let strings = ElfStrings::from_path(path).unwrap();
        assert!(!strings.sections.is_empty());
    }

    #[test]
    fn format_lookup_uses_noload_section_at_vma_zero() {
        let strings = ElfStrings {
            sections: vec![
                Section {
                    name: ".rodata".to_string(),
                    addr: 0x3f00_0000,
                    end: 0x3f00_0010,
                    data: b"TAG\0".to_vec(),
                },
                Section {
                    name: ".noload".to_string(),
                    addr: 0,
                    end: 16,
                    data: [0, 0, 0, 0, b'f', b'm', b't', b'\0'].to_vec(),
                },
            ],
            symbols: Vec::new(),
            lines: None,
            address_slide: AtomicU32::new(0),
        };

        assert_eq!(strings.read_format(4), Some("fmt"));
        assert_eq!(strings.read_tag(0x3f00_0000), Some("TAG"));
        assert_eq!(strings.read_tag(4), None);
    }

    #[test]
    fn format_lookup_falls_back_to_rodata_for_cpp_wrapper() {
        let strings = ElfStrings {
            sections: vec![Section {
                name: ".rodata".to_string(),
                addr: 0x3f00_0000,
                end: 0x3f00_0020,
                data: b"value={}\0TAG\0".to_vec(),
            }],
            symbols: Vec::new(),
            lines: None,
            address_slide: AtomicU32::new(0),
        };

        assert_eq!(strings.read_format(0x3f00_0000), Some("value={}"));
        assert_eq!(strings.read_tag(0x3f00_0009), Some("TAG"));

        strings.set_address_slide(0x0100_0000);
        assert_eq!(strings.read_format(0x4000_0000), Some("value={}"));
        assert_eq!(strings.read_tag(0x4000_0009), Some("TAG"));
    }

    #[test]
    fn ambiguous_lookup_with_different_strings_fails() {
        let strings = ElfStrings {
            sections: vec![
                Section {
                    name: ".noload".to_string(),
                    addr: 0,
                    end: 8,
                    data: b"\0\0\0\0a\0".to_vec(),
                },
                Section {
                    name: ".noload.extra".to_string(),
                    addr: 4,
                    end: 8,
                    data: b"b\0".to_vec(),
                },
            ],
            symbols: Vec::new(),
            lines: None,
            address_slide: AtomicU32::new(0),
        };

        assert_eq!(strings.read_format(4), None);
    }

    #[test]
    fn symbol_lookup_uses_containing_function() {
        let strings = ElfStrings {
            sections: Vec::new(),
            symbols: vec![
                Symbol {
                    name: "first".to_string(),
                    addr: 0x1000,
                    size: 0x20,
                },
                Symbol {
                    name: "second".to_string(),
                    addr: 0x1040,
                    size: 0,
                },
            ],
            lines: None,
            address_slide: AtomicU32::new(0),
        };

        assert_eq!(
            strings.resolve_symbol(0x1014),
            Some(ResolvedSymbol {
                name: "first",
                address: 0x1000,
                offset: 0x14,
            })
        );
        assert_eq!(strings.resolve_symbol(0x1030), None);
        assert_eq!(strings.resolve_symbol(0x1044).unwrap().name, "second");
    }
}
