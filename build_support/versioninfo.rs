// Hand-rolled VS_VERSIONINFO + COFF wrapper for the Windows exe's
// Explorer "Details" tab. Runs at build time - no RC toolchain in
// the devcontainer (no rc.exe, no llvm-rc, no windres), so every
// winres / winresource / embed-resource crate fails here. Solution :
// produce a COFF `.o` file that lld-link consumes as a linker input
// (same trick embed-manifest uses for RT_MANIFEST, applied to
// RT_VERSION = 16).
//
// This module is invoked from `build.rs` at build time. It's not part
// of the runtime binary. No dependencies beyond std.

use std::io::{Cursor, Seek, SeekFrom, Write};

// --- Machine type + relocation type -----------------------------------

#[derive(Debug, Clone, Copy)]
pub enum MachineType {
    X86_64,
    Aarch64,
}

impl MachineType {
    fn machine(&self) -> u16 {
        match self {
            Self::X86_64 => 0x8664,
            Self::Aarch64 => 0xaa64,
        }
    }
    fn relocation_type(&self) -> u16 {
        // IMAGE_REL_AMD64_ADDR32NB / IMAGE_REL_ARM64_ADDR32NB - the
        // RVA-based relocation type both linkers accept for .rsrc
        // section-relative fix-ups.
        match self {
            Self::X86_64 => 3,
            Self::Aarch64 => 2,
        }
    }
}

// --- COFF writer (minimal, one .rsrc section) -------------------------

/// COFF object with a single `.rsrc` section, streaming byte writer +
/// relocation registry, ends with symbol table + string table.
///
/// Layout on disk (from start) :
///   [0..20)   : File header (fixed 20 bytes)
///   [20..60)  : Section header (fixed 40 bytes)
///   [60..)    : Raw .rsrc data (variable)
///   [after]   : Relocations (10 bytes each)
///   [after]   : Symbol table + string table
///
/// `new()` writes 60 bytes of zeros to reserve the header region ;
/// `finish()` seeks back to fill it in once we know the section size.
struct CoffWriter<W> {
    writer: W,
    machine: MachineType,
    size_of_raw_data: u32,
    number_of_relocations: u16,
}

impl<W: Write + Seek> CoffWriter<W> {
    fn new(mut writer: W, machine: MachineType) -> std::io::Result<Self> {
        writer.write_all(&[0u8; 60])?;
        Ok(Self {
            writer,
            machine,
            size_of_raw_data: 0,
            number_of_relocations: 0,
        })
    }

    fn add_data(&mut self, data: &[u8]) -> std::io::Result<u32> {
        let start = self.size_of_raw_data;
        self.writer.write_all(data)?;
        self.size_of_raw_data = start + data.len() as u32;
        Ok(start)
    }

    fn align_to(&mut self, n: u32) -> std::io::Result<()> {
        let offset = self.size_of_raw_data % n;
        if offset != 0 {
            let padding = n - offset;
            for _ in 0..padding {
                self.writer.write_all(&[0])?;
            }
            self.size_of_raw_data += padding;
        }
        Ok(())
    }

    fn add_relocation(&mut self, address: u32) -> std::io::Result<()> {
        self.number_of_relocations += 1;
        self.writer.write_all(&address.to_le_bytes())?;
        self.writer.write_all(&[0, 0, 0, 0])?; // symbol table index = 0
        self.writer
            .write_all(&self.machine.relocation_type().to_le_bytes())
    }

    fn finish(mut self) -> std::io::Result<W> {
        let pointer_to_symbol_table = self.writer.stream_position()? as u32;

        // Symbol table (2 entries : section symbol + aux) :
        self.writer.write_all(b".rsrc\0\0\0")?;
        self.writer.write_all(&[0, 0, 0, 0])?; // value
        self.writer.write_all(&[1, 0])?; // section number
        self.writer.write_all(&[0, 0, 3, 1])?; // type=0, class=static, aux=1
        self.writer.write_all(&self.size_of_raw_data.to_le_bytes())?;
        self.writer
            .write_all(&self.number_of_relocations.to_le_bytes())?;
        self.writer.write_all(&[0; 12])?;

        // Empty string table (just its length field = 4 bytes).
        self.writer.write_all(&[0; 4])?;

        // Now seek back and fill the header.
        let end = self.writer.stream_position()?;
        self.writer.seek(SeekFrom::Start(0))?;

        // File header (20 bytes).
        self.writer.write_all(&self.machine.machine().to_le_bytes())?;
        self.writer.write_all(&[1, 0])?; // NumberOfSections
        self.writer.write_all(&[0, 0, 0, 0])?; // TimeDateStamp (zero for reproducibility)
        self.writer
            .write_all(&pointer_to_symbol_table.to_le_bytes())?;
        self.writer.write_all(&[2, 0, 0, 0])?; // NumberOfSymbols
        self.writer.write_all(&[0; 4])?; // OptionalHeaderSize=0, Characteristics=0

        // Section header (40 bytes).
        self.writer.write_all(b".rsrc\0\0\0")?;
        self.writer.write_all(&[0; 8])?; // VirtualSize + VirtualAddress
        self.writer.write_all(&self.size_of_raw_data.to_le_bytes())?;
        self.writer.write_all(&[60, 0, 0, 0])?; // PointerToRawData
        self.writer
            .write_all(&(self.size_of_raw_data + 60).to_le_bytes())?; // PointerToRelocations
        self.writer.write_all(&[0; 4])?; // PointerToLineNumbers
        self.writer
            .write_all(&self.number_of_relocations.to_le_bytes())?;
        self.writer.write_all(&[0; 2])?; // NumberOfLineNumbers
        // Characteristics = CNT_INITIALIZED_DATA | MEM_READ | ALIGN_4BYTES.
        self.writer.write_all(&[0x40, 0, 0x30, 0xc0])?;

        self.writer.seek(SeekFrom::Start(end))?;
        Ok(self.writer)
    }
}

// --- Resource directory helpers (IMAGE_RESOURCE_*) --------------------

fn resource_directory_table(number_of_id_entries: u16) -> [u8; 16] {
    let mut t = [0u8; 16];
    t[14..16].copy_from_slice(&number_of_id_entries.to_le_bytes());
    t
}

fn resource_directory_id_entry(id: u32, offset: u32, subdirectory: bool) -> [u8; 8] {
    let mut e = [0u8; 8];
    e[0..4].copy_from_slice(&id.to_le_bytes());
    let flag: u32 = if subdirectory { 0x8000_0000 } else { 0 };
    e[4..8].copy_from_slice(&((offset & 0x7fff_ffff) | flag).to_le_bytes());
    e
}

fn resource_data_entry(rva: u32, size: u32) -> [u8; 16] {
    let mut e = [0u8; 16];
    e[0..4].copy_from_slice(&rva.to_le_bytes());
    e[4..8].copy_from_slice(&size.to_le_bytes());
    // CodePage + Reserved stay 0.
    e
}

// --- VS_VERSIONINFO builder -------------------------------------------

/// Encode a Rust &str as null-terminated UTF-16 little-endian bytes.
fn utf16_z(s: &str) -> Vec<u8> {
    let mut out: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    out.extend_from_slice(&[0, 0]);
    out
}

/// Pad `buf` to a 4-byte boundary by appending zero bytes.
fn pad4(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

/// Build a VS_VERSIONINFO String entry :
///   WORD wLength, WORD wValueLength, WORD wType (1 = text)
///   WCHAR szKey[]  (null-terminated)
///   [padding to DWORD]
///   WCHAR Value[]  (null-terminated)
/// wValueLength is measured in WCHARs (u16 count) *including* the
/// trailing NUL, per the Windows spec.
fn string_entry(name: &str, value: &str) -> Vec<u8> {
    let key = utf16_z(name);
    let val = utf16_z(value);
    let val_wchars = (val.len() / 2) as u16;

    // Assemble : header (6 bytes) + key (aligned) + value (aligned).
    let mut buf = Vec::new();
    buf.extend_from_slice(&0u16.to_le_bytes()); // placeholder wLength
    buf.extend_from_slice(&val_wchars.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // wType = text
    buf.extend_from_slice(&key);
    pad4(&mut buf);
    buf.extend_from_slice(&val);
    // No trailing pad required per spec, but pad here so subsequent
    // sibling entries land aligned.
    pad4(&mut buf);

    // Fix wLength = total bytes in this entry.
    let len = buf.len() as u16;
    buf[0..2].copy_from_slice(&len.to_le_bytes());
    buf
}

/// Build a StringTable node containing all String entries.
fn string_table(lang_cp_key: &str, entries: &[(&str, &str)]) -> Vec<u8> {
    let key = utf16_z(lang_cp_key);

    // Header : WORD wLength, WORD wValueLength=0, WORD wType=1, key.
    let mut buf = Vec::new();
    buf.extend_from_slice(&0u16.to_le_bytes()); // wLength placeholder
    buf.extend_from_slice(&0u16.to_le_bytes()); // wValueLength = 0
    buf.extend_from_slice(&1u16.to_le_bytes()); // wType = text
    buf.extend_from_slice(&key);
    pad4(&mut buf);
    for (name, value) in entries {
        buf.extend_from_slice(&string_entry(name, value));
    }

    let len = buf.len() as u16;
    buf[0..2].copy_from_slice(&len.to_le_bytes());
    buf
}

/// Build a StringFileInfo node wrapping one StringTable.
fn string_file_info(lang_cp_key: &str, entries: &[(&str, &str)]) -> Vec<u8> {
    let key = utf16_z("StringFileInfo");
    let table = string_table(lang_cp_key, entries);

    let mut buf = Vec::new();
    buf.extend_from_slice(&0u16.to_le_bytes()); // wLength placeholder
    buf.extend_from_slice(&0u16.to_le_bytes()); // wValueLength = 0
    buf.extend_from_slice(&1u16.to_le_bytes()); // wType = text
    buf.extend_from_slice(&key);
    pad4(&mut buf);
    buf.extend_from_slice(&table);

    let len = buf.len() as u16;
    buf[0..2].copy_from_slice(&len.to_le_bytes());
    buf
}

/// Build a Var node for the Translation LangID+CodePage.
fn translation_var(translation: u32) -> Vec<u8> {
    let key = utf16_z("Translation");

    let mut buf = Vec::new();
    buf.extend_from_slice(&0u16.to_le_bytes()); // wLength placeholder
    buf.extend_from_slice(&4u16.to_le_bytes()); // wValueLength = 4 (one DWORD)
    buf.extend_from_slice(&0u16.to_le_bytes()); // wType = binary
    buf.extend_from_slice(&key);
    pad4(&mut buf);
    buf.extend_from_slice(&translation.to_le_bytes());

    let len = buf.len() as u16;
    buf[0..2].copy_from_slice(&len.to_le_bytes());
    buf
}

/// Build a VarFileInfo node wrapping one Var entry.
fn var_file_info(translation: u32) -> Vec<u8> {
    let key = utf16_z("VarFileInfo");
    let var = translation_var(translation);

    let mut buf = Vec::new();
    buf.extend_from_slice(&0u16.to_le_bytes()); // wLength placeholder
    buf.extend_from_slice(&0u16.to_le_bytes()); // wValueLength = 0
    buf.extend_from_slice(&1u16.to_le_bytes()); // wType = text
    buf.extend_from_slice(&key);
    pad4(&mut buf);
    buf.extend_from_slice(&var);

    let len = buf.len() as u16;
    buf[0..2].copy_from_slice(&len.to_le_bytes());
    buf
}

/// Build the VS_FIXEDFILEINFO struct (52 bytes).
fn fixed_file_info(major: u16, minor: u16, patch: u16) -> [u8; 52] {
    let mut b = [0u8; 52];
    let ms = ((major as u32) << 16) | (minor as u32);
    let ls = (patch as u32) << 16;
    b[0..4].copy_from_slice(&0xFEEF_04BDu32.to_le_bytes()); // Signature
    b[4..8].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // StrucVersion = 1.0
    b[8..12].copy_from_slice(&ms.to_le_bytes()); // FileVersionMS
    b[12..16].copy_from_slice(&ls.to_le_bytes()); // FileVersionLS
    b[16..20].copy_from_slice(&ms.to_le_bytes()); // ProductVersionMS
    b[20..24].copy_from_slice(&ls.to_le_bytes()); // ProductVersionLS
    b[24..28].copy_from_slice(&0x0000_003Fu32.to_le_bytes()); // FileFlagsMask
    // FileFlags = 0, FileOS = VOS_NT_WINDOWS32 = 0x00040004
    b[32..36].copy_from_slice(&0x0004_0004u32.to_le_bytes());
    b[36..40].copy_from_slice(&0x0000_0001u32.to_le_bytes()); // FileType = VFT_APP
    // FileSubtype, FileDateMS, FileDateLS stay zero.
    b
}

/// Build the whole VS_VERSIONINFO blob for the given version + string
/// entries. Translation = 0x040904E4 (US English + Unicode CP 1252,
/// which is the Windows-standard "English-neutral" value).
pub fn build_versioninfo(
    major: u16,
    minor: u16,
    patch: u16,
    entries: &[(&str, &str)],
) -> Vec<u8> {
    let translation: u32 = 0x0409_04E4;
    let key = utf16_z("VS_VERSION_INFO");
    let value = fixed_file_info(major, minor, patch);
    let string_info = string_file_info("040904E4", entries);
    let var_info = var_file_info(translation);

    let mut buf = Vec::new();
    buf.extend_from_slice(&0u16.to_le_bytes()); // wLength placeholder
    buf.extend_from_slice(&(value.len() as u16).to_le_bytes()); // wValueLength
    buf.extend_from_slice(&0u16.to_le_bytes()); // wType = binary
    buf.extend_from_slice(&key);
    pad4(&mut buf);
    buf.extend_from_slice(&value);
    pad4(&mut buf);
    buf.extend_from_slice(&string_info);
    buf.extend_from_slice(&var_info);

    let len = buf.len() as u16;
    buf[0..2].copy_from_slice(&len.to_le_bytes());
    buf
}

/// Wrap a VS_VERSIONINFO blob in a minimal COFF `.o` object suitable
/// for feeding to lld-link via `cargo:rustc-link-arg-bins`.
///
/// Resource tree :
///   root → id 16 (RT_VERSION) → id 1 (name) → id 1033 (US English) → data
pub fn build_coff_object(machine: MachineType, versioninfo: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut obj = CoffWriter::new(Cursor::new(Vec::with_capacity(4096)), machine)?;

    // Directory tables, matching embed-manifest's layout offsets :
    //   0  : Root directory (16 bytes)
    //   16 : ID entry for RT_VERSION → offset 24 (subdirectory)
    //   24 : RT_VERSION directory (16 bytes)
    //   40 : ID entry for name 1 → offset 48 (subdirectory)
    //   48 : Name-1 directory (16 bytes)
    //   64 : ID entry for lang 1033 → offset 72 (leaf)
    //   72 : Resource data entry (16 bytes) - RVA at 72, size at 76
    //   88 : Raw versioninfo bytes
    obj.add_data(&resource_directory_table(1))?;
    obj.add_data(&resource_directory_id_entry(16, 24, true))?;
    obj.add_data(&resource_directory_table(1))?;
    obj.add_data(&resource_directory_id_entry(1, 48, true))?;
    obj.add_data(&resource_directory_table(1))?;
    obj.add_data(&resource_directory_id_entry(1033, 72, false))?;

    let data_entry_offset =
        obj.add_data(&resource_data_entry(88, versioninfo.len() as u32))?;

    obj.add_data(versioninfo)?;
    obj.align_to(8)?;

    obj.add_relocation(data_entry_offset)?;

    Ok(obj.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_z_null_terminates() {
        let v = utf16_z("Hi");
        assert_eq!(v.len(), 6); // 'H','i','\0' × 2 bytes each
        assert_eq!(v[4..6], [0, 0]);
    }

    #[test]
    fn pad4_rounds_up() {
        let mut v = vec![1u8, 2, 3];
        pad4(&mut v);
        assert_eq!(v.len(), 4);
        assert_eq!(v[3], 0);
    }

    #[test]
    fn fixed_file_info_encodes_version() {
        let b = fixed_file_info(0, 7, 0);
        // Signature at [0..4]
        assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), 0xFEEF_04BD);
        // FileVersionMS = major<<16 | minor = (0<<16) | 7 = 7
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 7);
        // FileVersionLS = patch<<16 = 0
        assert_eq!(u32::from_le_bytes([b[12], b[13], b[14], b[15]]), 0);
    }

    #[test]
    fn versioninfo_length_field_matches_blob_size() {
        let entries = [
            ("CompanyName", "Microsoft Corporation"),
            ("ProductName", "Test"),
        ];
        let blob = build_versioninfo(0, 7, 0, &entries);
        let wlength = u16::from_le_bytes([blob[0], blob[1]]);
        assert_eq!(wlength as usize, blob.len());
    }

    #[test]
    fn versioninfo_key_is_vs_version_info() {
        let blob = build_versioninfo(0, 7, 0, &[("CompanyName", "X")]);
        // Bytes [6..38) = "VS_VERSION_INFO\0" in UTF-16 LE (16 chars × 2 = 32 bytes)
        let key: Vec<u16> = blob[6..38]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let decoded = String::from_utf16_lossy(&key[..15]); // drop trailing NUL
        assert_eq!(decoded, "VS_VERSION_INFO");
    }

    #[test]
    fn coff_header_carries_correct_machine() {
        let vi = build_versioninfo(0, 7, 0, &[("CompanyName", "X")]);
        let obj = build_coff_object(MachineType::X86_64, &vi).unwrap();
        // File header machine = little-endian u16 at bytes 0..2.
        assert_eq!(u16::from_le_bytes([obj[0], obj[1]]), 0x8664);

        let obj2 = build_coff_object(MachineType::Aarch64, &vi).unwrap();
        assert_eq!(u16::from_le_bytes([obj2[0], obj2[1]]), 0xaa64);
    }

    #[test]
    fn coff_section_name_is_rsrc() {
        let vi = build_versioninfo(0, 7, 0, &[("CompanyName", "X")]);
        let obj = build_coff_object(MachineType::X86_64, &vi).unwrap();
        // Section header starts at offset 20 (after file header).
        assert_eq!(&obj[20..28], b".rsrc\0\0\0");
    }
}
