use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, Result};
use memmap2::Mmap;
use ring::digest::{Context, Digest, SHA256};

use crate::BuildId;
use object::elf::{FileHeader32, FileHeader64, PT_LOAD};
use object::read::elf::FileHeader;
use object::read::elf::ProgramHeader;
use object::Endianness;
use object::FileKind;
use object::Object;
use object::ObjectKind;
use object::ObjectSection;

/// Compact identifier for executable files.
///
/// Compact identifier for executable files derived from the first 8 bytes
/// of the hash of the code stored in the .text ELF segment. By using this
/// smaller type for object files less memory is used and also comparison,
/// and other operations are cheaper.
pub type ExecutableId = u64;

/// Elf load segments used during address normalization to find the segment
/// for what an code address falls into.
#[derive(Debug, Clone)]
pub struct ElfLoad {
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
}

#[derive(Debug)]
pub struct ObjectFile {
    /// Warning! `object` must always go above `mmap` to ensure it will be dropped
    /// before. Rust guarantees that fields are dropped in the order they are defined.
    object: object::File<'static>, // Its lifetime is tied to the `mmap` below.
    mmap: Box<Mmap>,
    build_id: BuildId,
}

impl ObjectFile {
    pub fn new(path: &Path) -> Result<Self> {
        let file = fs::File::open(path)?;
        // Rust offers no guarantees on whether a "move" is done virtually or by memcpying,
        // so to ensure that the memory value is valid we store it in the heap.
        // Safety: Memory mapping files can cause issues if the file is modified or unmapped.
        let mmap = Box::new(unsafe { Mmap::map(&file) }?);
        let object = object::File::parse(&**mmap)?;
        // Safety: The lifetime of `object` will outlive `mmap`'s. We ensure `mmap` lives as long as
        // `object` by defining `object` before.
        let object =
            unsafe { std::mem::transmute::<object::File<'_>, object::File<'static>>(object) };
        let build_id = Self::read_build_id(&object)?;

        Ok(ObjectFile {
            object,
            mmap,
            build_id,
        })
    }

    /// Returns an identifier for the executable using the first 8 bytes of the build id.
    pub fn id(&self) -> Result<ExecutableId> {
        Ok(u64::from_ne_bytes(self.build_id.data[..8].try_into()?))
    }

    /// Returns the executable build ID.
    pub fn build_id(&self) -> &BuildId {
        &self.build_id
    }

    /// Returns the executable build ID if present. If no GNU build ID and no Go build ID
    /// are found it returns the hash of the text section.
    pub fn read_build_id(object: &object::File<'static>) -> Result<BuildId> {
        let gnu_build_id = object.build_id()?;

        if let Some(data) = gnu_build_id {
            return Ok(BuildId::gnu_from_bytes(data));
        }

        // Golang (the Go toolchain does not interpret these bytes as we do).
        for section in object.sections() {
            if section.name()? == ".note.go.buildid" {
                if let Ok(data) = section.data() {
                    return Ok(BuildId::go_from_bytes(data));
                }
            }
        }

        // No build id (Rust, some compilers and Linux distributions).
        let Some(code_hash) = code_hash(object) else {
            return Err(anyhow!("code hash is None"));
        };
        Ok(BuildId::sha256_from_digest(&code_hash))
    }

    /// Returns whether the object has debug symbols.
    pub fn has_debug_info(&self) -> bool {
        self.object.has_debug_symbols()
    }

    pub fn is_dynamic(&self) -> bool {
        self.object.kind() == ObjectKind::Dynamic
    }

    pub fn is_go(&self) -> bool {
        for section in self.object.sections() {
            if let Ok(section_name) = section.name() {
                if section_name == ".gosymtab"
                    || section_name == ".gopclntab"
                    || section_name == ".note.go.buildid"
                {
                    return true;
                }
            }
        }
        false
    }

    pub fn elf_load_segments(&self) -> Result<Vec<ElfLoad>> {
        let mmap = &**self.mmap;

        match FileKind::parse(mmap) {
            Ok(FileKind::Elf32) => {
                let header: &FileHeader32<Endianness> = FileHeader32::<Endianness>::parse(mmap)?;
                let endian = header.endian()?;
                let segments = header.program_headers(endian, mmap)?;

                let mut elf_loads = Vec::new();
                for segment in segments {
                    if segment.p_type(endian) == PT_LOAD {
                        elf_loads.push(ElfLoad {
                            p_offset: segment.p_offset(endian) as u64,
                            p_vaddr: segment.p_vaddr(endian) as u64,
                            p_filesz: segment.p_filesz(endian) as u64,
                        });
                    }
                }
                Ok(elf_loads)
            }
            Ok(FileKind::Elf64) => {
                let header: &FileHeader64<Endianness> = FileHeader64::<Endianness>::parse(mmap)?;
                let endian = header.endian()?;
                let segments = header.program_headers(endian, mmap)?;

                let mut elf_loads = Vec::new();
                for segment in segments {
                    if segment.p_type(endian) == PT_LOAD {
                        elf_loads.push(ElfLoad {
                            p_offset: segment.p_offset(endian),
                            p_vaddr: segment.p_vaddr(endian),
                            p_filesz: segment.p_filesz(endian),
                        });
                    }
                }
                Ok(elf_loads)
            }
            Ok(other_file_kind) => Err(anyhow!(
                "object is not an 32 or 64 bits ELF but {:?}",
                other_file_kind
            )),
            Err(e) => Err(anyhow!("FileKind failed with {:?}", e)),
        }
    }
}

pub fn code_hash(object: &object::File) -> Option<Digest> {
    for section in object.sections() {
        let Ok(section_name) = section.name() else {
            continue;
        };

        if section_name == ".text" {
            if let Ok(section) = section.data() {
                return Some(sha256_digest(section));
            }
        }
    }

    None
}

fn sha256_digest<R: Read>(mut reader: R) -> Digest {
    let mut context = Context::new(&SHA256);
    let mut buffer = [0; 1024];

    loop {
        let count = reader
            .read(&mut buffer)
            .expect("reading digest into buffer should not fail");
        if count == 0 {
            break;
        }
        context.update(&buffer[..count]);
    }

    context.finish()
}
