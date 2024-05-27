use anyhow::Result;
use gimli::{CfaRule, CieOrFde, EhFrame, UnwindContext, UnwindSection};
use lazy_static::lazy_static;
use memmap2::Mmap;
use object::{Object, ObjectSection, Section};
use std::fs::File;
use std::path::PathBuf;
use thiserror::Error;

use crate::bpf::profiler_bindings::stack_unwind_row_t;
use anyhow::anyhow;
use tracing::{error, span, Level};

#[repr(u8)]
pub enum CfaType {
    // Unknown = 0,
    FramePointerOffset = 1,
    StackPointerOffset = 2,
    Expression = 3,
    EndFdeMarker = 4,
    UnsupportedRegisterOffset = 5,
    OffsetDidNotFit = 6,
}

#[repr(u8)]
enum RbpType {
    // Unknown = 0,
    CfaOffset = 1,
    Register = 2,
    Expression = 3,
    UndefinedReturnAddress = 4,
}

#[repr(u16)]
enum PltType {
    // Unknown = 0,
    Plt1 = 1,
    Plt2 = 2,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct CompactUnwindRow {
    pub pc: u64,
    // pub ra: u16,
    pub cfa_type: u8,
    pub rbp_type: u8,
    pub cfa_offset: u16,
    pub rbp_offset: i16,
}

lazy_static! {
    static ref PLT1: [u8; 11] = [
        gimli::constants::DW_OP_breg7,
        gimli::constants::DW_OP_const1u,
        gimli::constants::DW_OP_breg16,
        gimli::DwOp(0), // ?
        gimli::constants::DW_OP_lit15,
        gimli::constants::DW_OP_and,
        gimli::constants::DW_OP_lit11,
        gimli::constants::DW_OP_ge,
        gimli::constants::DW_OP_lit3,
        gimli::constants::DW_OP_shl,
        gimli::constants::DW_OP_plus,
    ].map(|a| a.0);

    static ref PLT2: [u8; 11] = [
        gimli::constants::DW_OP_breg7,
        gimli::constants::DW_OP_const1u,
        gimli::constants::DW_OP_breg16,
        gimli::DwOp(0), // ?
        gimli::constants::DW_OP_lit15,
        gimli::constants::DW_OP_and,
        gimli::constants::DW_OP_lit10,
        gimli::constants::DW_OP_ge,
        gimli::constants::DW_OP_lit3,
        gimli::constants::DW_OP_shl,
        gimli::constants::DW_OP_plus,
    ].map(|a| a.0);
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("no eh_frame section found")]
    ErrorNoEhFrameSection,
    #[error("object file could not be parsed")]
    ErrorParsingFile,
    #[error("no text section found")]
    ErrorNoTextSection,
}

const RBP_X86: gimli::Register = gimli::Register(6);
const RSP_X86: gimli::Register = gimli::Register(7);

pub fn end_of_function_marker(last_addr: u64) -> CompactUnwindRow {
    CompactUnwindRow {
        pc: last_addr,
        cfa_type: CfaType::EndFdeMarker as u8,
        ..Default::default()
    }
}

// NOTE: Why do we have two variants of unwind data?
pub enum UnwindData {
    // Initial, end addresses
    // NOTE: Initial and end address of what?? A given function?
    Function(u64, u64),
    Instruction(CompactUnwindRow),
}

pub fn compact_printing_callback(unwind_data: &UnwindData) {
    match unwind_data {
        UnwindData::Function(begin, end) => {
            println!("=> Function start: {:x}, Function end: {:x}", begin, end);
        }
        UnwindData::Instruction(compact_row) => {
            println!(
                "\tpc: {:x} cfa_type: {:<2} rbp_type: {:<2} cfa_offset: {:<4} rbp_offset: {:<4}",
                compact_row.pc,
                compact_row.cfa_type,
                compact_row.rbp_type,
                compact_row.cfa_offset,
                compact_row.rbp_offset
            );
        }
    }
}

/// Just used for debugging.
pub fn log_unwind_info_sections(path: &PathBuf) -> Result<()> {
    use tracing::info;

    let file = File::open(path)?;
    let mmap = unsafe { memmap2::Mmap::map(&file)? };
    let object_file = object::File::parse(&mmap[..])?;

    let eh_frame_section = object_file.section_by_name(".eh_frame");
    let dunder_eh_frame_section = object_file.section_by_name(".__eh_frame");
    let debug_frame_section = object_file.section_by_name(".debug_frame");
    let dunder_zdebug_frame_section = object_file.section_by_name(".__zdebug_frame");

    fn get_size(s: Option<Section>) -> Option<u64> {
        s.as_ref()?;

        Some(s.expect("should never happen").size())
    }

    info!(
        "Unwind info sizes for object {:?} .eh_frame: {:?} .__eh_frame: {:?} .debug_frame: {:?} .__zdebug_frame {:?}",
        path,
        get_size(eh_frame_section),
        get_size(dunder_eh_frame_section),
        get_size(debug_frame_section),
        get_size(dunder_zdebug_frame_section)
    );

    Ok(())
}

// Ideally this interface should do most of the preparatory work in the
// constructor but this is complicated by the various lifetimes.

// NOTE: What are the various lifetimes being referred to here?
// What is this used for? Building unwind info for a given executable at a given path?
pub struct UnwindInfoBuilder<'a> {
    mmap: Mmap,
    callback: Box<dyn FnMut(&UnwindData) + 'a>,
}

// Initializes a struct
impl<'a> UnwindInfoBuilder<'a> {
    // NOTE: Does this just mmap what located at the supplied path
    // and then uses it to initialize the UnwindInfoBuilder struct??

    pub fn with_callback(
        path: &'a str,
        callback: impl FnMut(&UnwindData) + 'a,
    ) -> anyhow::Result<Self> {
        let in_file = File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&in_file)? };

        Ok(Self {
            mmap,
            callback: Box::new(callback),
        })
    }

    // NOTE: What does this method do?
    // Call the unwind info builder, get some data,
    // process each entry using a callback which
    // uses the callback parameter to generate and add
    // and unwindinfo row to a result table.
    pub fn to_vec(path: &str) -> anyhow::Result<Vec<CompactUnwindRow>> {
        let mut result = Vec::new();

        // NOTE: Why do we need to keep track of the last function
        // end address?
        let mut last_function_end_addr: Option<u64> = None;

        let builder = UnwindInfoBuilder::with_callback(path, |unwind_data| {
            match unwind_data {
                // If a function address variant is passed into the unwindInfoBuilder callback
                // Do some work to build the CompactUnwindRow object
                UnwindData::Function(_, end_addr) => {
                    // Add a function marker for the previous function.
                    // NOTE: What is a function marker?
                    if let Some(addr) = last_function_end_addr {
                        // NOTE: there's a value for the last function end address
                        // use that to create an unwind table entry representing
                        // the end of a function
                        // Why is this necessary?
                        let marker = end_of_function_marker(addr);
                        let row = CompactUnwindRow {
                            pc: marker.pc,
                            cfa_offset: marker.cfa_offset,
                            cfa_type: marker.cfa_type,
                            rbp_type: marker.rbp_type,
                            rbp_offset: marker.rbp_offset,
                        };
                        result.push(row)
                    }
                    last_function_end_addr = Some(*end_addr);
                }
                // If it is an unwindrow struct, simply add a copy
                // of the struct to the unwind info table --
                // which is vector of unwind info structs. 
                UnwindData::Instruction(compact_row) => {
                    let row = CompactUnwindRow {
                        pc: compact_row.pc,
                        cfa_offset: compact_row.cfa_offset,
                        cfa_type: compact_row.cfa_type,
                        rbp_type: compact_row.rbp_type,
                        rbp_offset: compact_row.rbp_offset,
                    };
                    result.push(row);
                }
            }
        });

        builder?.process()?;

        // Add marker for the last function.
        if let Some(last_addr) = last_function_end_addr {
            let marker = end_of_function_marker(last_addr);
            let row = CompactUnwindRow {
                pc: marker.pc,
                cfa_offset: marker.cfa_offset,
                cfa_type: marker.cfa_type,
                rbp_type: marker.rbp_type,
                rbp_offset: marker.rbp_offset,
            };
            result.push(row);
        }

        Ok(result)
    }

    pub fn process(mut self) -> Result<(), anyhow::Error> {
        // TODO: Is this always guaranteed to succeed?
        let object_file = object::File::parse(&self.mmap[..])?;
        
        // NOTE: Get the eh_frame section
        let eh_frame_section = object_file
            .section_by_name(".eh_frame")
            .ok_or(Error::ErrorNoEhFrameSection)?;
        
        // NOTE: Get the text section
        let text = object_file
            .section_by_name(".text")
            .ok_or(Error::ErrorNoTextSection)?;

        // NOTE: Get the addresses to be used as the base for the DW_EH_PE_xxx encoded pointers
        let bases = gimli::BaseAddresses::default()
            .set_eh_frame(eh_frame_section.address())
            .set_text(text.address());
        
        // NOTE: Why do we need to know the endianness of the object file?
        let endian = if object_file.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        let eh_frame_data = &eh_frame_section.uncompressed_data()?;

        let eh_frame = EhFrame::new(eh_frame_data, endian);
        let mut entries_iter = eh_frame.entries(&bases);

        let mut cur_cie = None;

        // This is a vector of pairs of program counter and FDE offsets
        // NOTE: Why do we need to build this vector?
        // NOTE: FDE offset is relative to what?
        let mut pc_and_fde_offset = Vec::new();
        
        // At this point, we've loaded the object file using the object crate, 
        // and parsed its eh_frame information using the gimli crate
        // Next iterate over the entries in the .eh_frame section, then do what?
        // NOTE: It looks like we're getting the FDE offsets here from the ehframe entries
        while let Ok(Some(entry)) = entries_iter.next() {
            match entry {
                CieOrFde::Cie(cie) => {
                    // NOTE: In what cases can there be multiple CIEs in a given object file?
                    cur_cie = Some(cie);
                }
                CieOrFde::Fde(partial_fde) => {
                    // TODO: Continue from here*****
                    let fde = partial_fde.parse(|eh_frame, bases, cie_offset| {
                        if let Some(cie) = &cur_cie {
                            // What does this mean?
                            // NOTE: Does the CIE always have to be at offset zero from it's starting position?
                            if cie.offset() == cie_offset.0 {
                                return Ok(cie.clone());
                            }
                        }
                        let cie = eh_frame.cie_from_offset(bases, cie_offset);
                        if let Ok(cie) = &cie {
                            cur_cie = Some(cie.clone());
                        }
                        cie
                    });
                    
                    // NOTE: This is the rust if let syntax
                    if let Ok(fde) = fde {
                        pc_and_fde_offset.push((fde.initial_address(), fde.offset()));
                    }
                }
            }
        }

        // NOTE: A span is a period of time when execution happened with a given context
        // It can be used used to track the interval from when a unit of work begins
        // to when it ends.
        let span = span!(Level::DEBUG, "sort pc and fdes").entered();

        // NOTE: Why do we need to sort the PC and FDEs?
        // Also, why did we need to build up this vecotr in the first place?
        pc_and_fde_offset.sort_by_key(|(pc, _)| *pc);
        span.exit();

        // TODO: Notes from the docs
        // "To avoid re-allocating the context multiple times when evaluating multiple CFI programs,
        // the same UnwindContext can be reused for multiple unwinds"
        // Does this apply here as well?
        let mut ctx = Box::new(UnwindContext::new());
        for (_, fde_offset) in pc_and_fde_offset {
            // Then now we get the actual FDEs using the offsets?
            // Why wasn't this done at the same time as the step above?
            let fde = eh_frame.fde_from_offset(
                &bases,
                gimli::EhFrameOffset(fde_offset),
                EhFrame::cie_from_offset,
            )?;
            

            // NOTE: Pass the function start and end address to the callback?
            // What's this used for? And why is it always called?
            (self.callback)(&UnwindData::Function(
                fde.initial_address(),
                fde.initial_address() + fde.len(),
            ));

            // NOTE: Why does a single FDE have multiple rows?
            let mut table = fde.rows(&eh_frame, &bases, &mut ctx)?;

            loop {
                let mut compact_row = CompactUnwindRow::default();

                match table.next_row() {
                    Ok(None) => break,
                    Ok(Some(row)) => {
                        compact_row.pc = row.start_address();
                        match row.cfa() {
                            CfaRule::RegisterAndOffset { register, offset } => {
                                if register == &RBP_X86 {
                                    compact_row.cfa_type = CfaType::FramePointerOffset as u8;
                                } else if register == &RSP_X86 {
                                    compact_row.cfa_type = CfaType::StackPointerOffset as u8;
                                } else {
                                    compact_row.cfa_type = CfaType::UnsupportedRegisterOffset as u8;
                                }

                                match u16::try_from(*offset) {
                                    Ok(off) => {
                                        compact_row.cfa_offset = off;
                                    }
                                    Err(_) => {
                                        compact_row.cfa_type = CfaType::OffsetDidNotFit as u8;
                                    }
                                }
                            }
                            CfaRule::Expression(exp) => {
                                let found_expression = exp
                                    .get(&eh_frame)
                                    .expect("getting the expression should never fail")
                                    .0
                                    .slice();

                                if found_expression == *PLT1 {
                                    compact_row.cfa_offset = PltType::Plt1 as u16;
                                } else if found_expression == *PLT2 {
                                    compact_row.cfa_offset = PltType::Plt2 as u16;
                                }

                                compact_row.cfa_type = CfaType::Expression as u8;
                            }
                        };

                        match row.register(RBP_X86) {
                            gimli::RegisterRule::Undefined => {}
                            gimli::RegisterRule::Offset(offset) => {
                                compact_row.rbp_type = RbpType::CfaOffset as u8;
                                compact_row.rbp_offset =
                                    i16::try_from(offset).expect("convert rbp offset");
                            }
                            gimli::RegisterRule::Register(_reg) => {
                                compact_row.rbp_type = RbpType::Register as u8;
                            }
                            gimli::RegisterRule::Expression(_) => {
                                compact_row.rbp_type = RbpType::Expression as u8;
                            }
                            _ => {
                                // print!(", rbp unsupported {:?}", rbp);
                            }
                        }

                        if row.register(fde.cie().return_address_register())
                            == gimli::RegisterRule::Undefined
                        {
                            compact_row.rbp_type = RbpType::UndefinedReturnAddress as u8;
                        }

                        // print!(", ra {:?}", row.register(fde.cie().return_address_register()));
                    }
                    _ => continue,
                }

                (self.callback)(&UnwindData::Instruction(compact_row));
            }
            // start_addresses.push(fde.initial_address() as u32);
            // end_addresses.push((fde.initial_address() + fde.len()) as u32);
        }
        Ok(())
    }
}

// Must be sorted.
pub fn remove_unnecesary_markers(unwind_info: &mut Vec<stack_unwind_row_t>) {
    let mut last_row: Option<stack_unwind_row_t> = None;
    let mut new_i: usize = 0;

    for i in 0..unwind_info.len() {
        let row = unwind_info[i];

        if let Some(last_row_unwrapped) = last_row {
            let previous_is_redundant_marker = (last_row_unwrapped.cfa_type
                == CfaType::EndFdeMarker as u8)
                && last_row_unwrapped.pc == row.pc;
            if previous_is_redundant_marker {
                new_i -= 1;
            }
        }

        let mut current_is_redundant_marker = false;
        if let Some(last_row_unwrapped) = last_row {
            current_is_redundant_marker =
                (row.cfa_type == CfaType::EndFdeMarker as u8) && last_row_unwrapped.pc == row.pc;
        }

        if !current_is_redundant_marker {
            unwind_info[new_i] = row;
            new_i += 1;
        }

        last_row = Some(row);
    }

    unwind_info.truncate(new_i);
}

// Must be sorted.
pub fn remove_redundant(unwind_info: &mut Vec<stack_unwind_row_t>) {
    let mut last_row: Option<stack_unwind_row_t> = None;
    let mut new_i: usize = 0;

    for i in 0..unwind_info.len() {
        let mut redundant = false;
        let row = unwind_info[i];

        if let Some(last_row_unwrapped) = last_row {
            redundant = row.cfa_type == last_row_unwrapped.cfa_type
                && row.cfa_offset == last_row_unwrapped.cfa_offset
                && row.rbp_type == last_row_unwrapped.rbp_type
                && row.rbp_offset == last_row_unwrapped.rbp_offset;
        }

        if !redundant {
            unwind_info[new_i] = row;
            new_i += 1;
        }

        last_row = Some(row);
    }

    unwind_info.truncate(new_i);
}

pub fn in_memory_unwind_info(path: &str) -> anyhow::Result<Vec<stack_unwind_row_t>> {
    let mut unwind_info: Vec<stack_unwind_row_t> = Vec::new();
    let mut last_function_end_addr: Option<u64> = None;
    let mut last_row = None;

    let builder = UnwindInfoBuilder::with_callback(path, |unwind_data| {
        match unwind_data {
            UnwindData::Function(_, end_addr) => {
                // Add the end addr when we hit a new func
                match last_function_end_addr {
                    Some(addr) => {
                        let marker = end_of_function_marker(addr);

                        let row: stack_unwind_row_t = stack_unwind_row_t {
                            pc: marker.pc,
                            cfa_offset: marker.cfa_offset,
                            cfa_type: marker.cfa_type,
                            rbp_type: marker.rbp_type,
                            rbp_offset: marker.rbp_offset,
                        };
                        unwind_info.push(row)
                    }
                    None => {
                        // todo: cleanup
                    }
                }
                last_function_end_addr = Some(*end_addr);
            }
            UnwindData::Instruction(compact_row) => {
                let row = stack_unwind_row_t {
                    pc: compact_row.pc,
                    cfa_offset: compact_row.cfa_offset,
                    cfa_type: compact_row.cfa_type,
                    rbp_type: compact_row.rbp_type,
                    rbp_offset: compact_row.rbp_offset,
                };
                unwind_info.push(row);
                last_row = Some(*compact_row)
            }
        }
    });

    builder?.process()?;

    if last_function_end_addr.is_none() {
        error!("no last func end addr");
        return Err(anyhow!("not sure what's going on"));
    }

    // Add the last marker
    let marker: CompactUnwindRow = end_of_function_marker(last_function_end_addr.unwrap());
    let row = stack_unwind_row_t {
        pc: marker.pc,
        cfa_offset: marker.cfa_offset,
        cfa_type: marker.cfa_type,
        rbp_type: marker.rbp_type,
        rbp_offset: marker.rbp_offset,
    };
    unwind_info.push(row);

    Ok(unwind_info)
}
