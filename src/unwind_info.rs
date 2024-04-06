use anyhow::Result;
use gimli::{CfaRule, CieOrFde, EhFrame, UnwindContext, UnwindSection};
use lazy_static::lazy_static;
use memmap2::Mmap;
use object::{Object, ObjectSection};
use std::fs::File;
use thiserror::Error;

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

pub enum UnwindData {
    // Initial, end addresses
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

// Ideally this interface should do most of the preparatory work in the
// constructor but this is complicated by the various lifetimes.
pub struct UnwindInfoBuilder<'a> {
    mmap: Mmap,
    callback: Box<dyn FnMut(&UnwindData) + 'a>,
}

impl<'a> UnwindInfoBuilder<'a> {
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

    pub fn to_vec(path: &str) -> anyhow::Result<Vec<CompactUnwindRow>> {
        let mut result = Vec::new();
        let mut last_function_end_addr: Option<u64> = None;

        let builder = UnwindInfoBuilder::with_callback(path, |unwind_data| {
            match unwind_data {
                UnwindData::Function(_, end_addr) => {
                    // Add a function marker for the previous function.
                    if let Some(addr) = last_function_end_addr {
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
        let object_file = object::File::parse(&self.mmap[..])?;
        let eh_frame_section = object_file
            .section_by_name(".eh_frame")
            .ok_or(Error::ErrorNoEhFrameSection)?;
        let text = object_file
            .section_by_name(".text")
            .ok_or(Error::ErrorNoTextSection)?;

        let bases = gimli::BaseAddresses::default()
            .set_eh_frame(eh_frame_section.address())
            .set_text(text.address());

        let endian = if object_file.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        let eh_frame_data = &eh_frame_section.uncompressed_data()?;

        let eh_frame = EhFrame::new(eh_frame_data, endian);
        let mut entries_iter = eh_frame.entries(&bases);

        let mut ctx = Box::new(UnwindContext::new());
        let mut cur_cie = None;

        while let Ok(Some(entry)) = entries_iter.next() {
            match entry {
                CieOrFde::Cie(cie) => {
                    cur_cie = Some(cie);
                }
                CieOrFde::Fde(partial_fde) => {
                    if let Ok(fde) = partial_fde.parse(|eh_frame, bases, cie_offset| {
                        if let Some(cie) = &cur_cie {
                            if cie.offset() == cie_offset.0 {
                                return Ok(cie.clone());
                            }
                        }
                        let cie = eh_frame.cie_from_offset(bases, cie_offset);
                        if let Ok(cie) = &cie {
                            cur_cie = Some(cie.clone());
                        }
                        cie
                    }) {
                        (self.callback)(&UnwindData::Function(
                            fde.initial_address(),
                            fde.initial_address() + fde.len(),
                        ));

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
                                                compact_row.cfa_type =
                                                    CfaType::FramePointerOffset as u8;
                                            } else if register == &RSP_X86 {
                                                compact_row.cfa_type =
                                                    CfaType::StackPointerOffset as u8;
                                            } else {
                                                compact_row.cfa_type =
                                                    CfaType::UnsupportedRegisterOffset as u8;
                                            }

                                            match u16::try_from(*offset) {
                                                Ok(off) => {
                                                    compact_row.cfa_offset = off;
                                                }
                                                Err(_) => {
                                                    compact_row.cfa_type =
                                                        CfaType::OffsetDidNotFit as u8;
                                                }
                                            }
                                        }
                                        CfaRule::Expression(exp) => {
                                            let found_expression = exp.0.slice();

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
                                        compact_row.rbp_type =
                                            RbpType::UndefinedReturnAddress as u8;
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
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::profiler::{in_memory_unwind_info, SHARD_CAPACITY};

    use super::*;
    use crate::bpf::profiler_bindings::chunk_info_t;
    use crate::bpf::profiler_bindings::stack_unwind_row_t;
    use crate::profiler::remove_redundant;
    use crate::profiler::remove_unnecesary_markers;
    use std::collections::HashMap;

    fn oracle_binary_search(
        unwind_info: &Vec<stack_unwind_row_t>,
        pc: u64,
    ) -> Option<stack_unwind_row_t> {
        let insertion_index = unwind_info.binary_search_by(|el| {
            // Prevent unaligned accesses.
            let this_pc = el.pc;
            this_pc.cmp(&pc)
        });

        match insertion_index {
            Ok(index) => Some(unwind_info[index]),
            Err(index) => {
                if index > 0 && index <= unwind_info.len() {
                    Some(unwind_info[index - 1])
                } else {
                    None
                }
            }
        }
    }

    fn our_binary_search(
        unwind_info: &Vec<stack_unwind_row_t>,
        pc: u64,
    ) -> Option<stack_unwind_row_t> {
        let mut left: u32 = 0;
        let mut right: u32 = unwind_info.len() as u32;
        let mut found = 0xFFFFFFFF;

        loop {
            if left >= right {
                if found == 0xFFFFFFFF {
                    return None;
                }
                if found >= unwind_info.len() {
                    return None;
                } else {
                    return Some(unwind_info[found]);
                }
            }
            let mid = (left + right) / 2;
            let this_pc = unwind_info[mid as usize].pc;
            if this_pc <= pc {
                found = mid as usize;
                left = mid + 1;
            } else {
                right = mid;
            }
        }
    }

    #[test]
    fn test_binary_search() {
        let mut unwind_info: Vec<_> = (0..SHARD_CAPACITY)
            .map(|i| stack_unwind_row_t {
                pc: 100 + i as u64,
                cfa_type: if i % 2 == 0 {
                    CfaType::StackPointerOffset
                } else {
                    CfaType::FramePointerOffset
                } as u8,
                cfa_offset: if i % 3 == 0 { 8 } else { 16 },
                rbp_type: RbpType::CfaOffset as u8,
                rbp_offset: 0,
            })
            .collect::<Vec<_>>();

        unwind_info.sort_by(|a, b| {
            let a_pc = a.pc;
            let b_pc = b.pc;
            a_pc.cmp(&b_pc)
        });
        let unwind_info = remove_unnecesary_markers(&unwind_info);
        let unwind_info = remove_redundant(&unwind_info);

        let mut first_pc = unwind_info[0].pc;
        let mut last_pc = unwind_info[unwind_info.len() - 1].pc;

        // Test edge conditions.
        if first_pc >= 2 {
            first_pc -= 2;
        }
        last_pc += 30;

        for pc in first_pc..last_pc {
            let oracle = oracle_binary_search(&unwind_info, pc);
            let ours = our_binary_search(&unwind_info, pc);

            if let Some(oo) = oracle {
                if oo.cfa_type == CfaType::EndFdeMarker as u8 {
                    continue;
                }
            }
            if let Some(oo) = ours {
                if oo.cfa_type == CfaType::EndFdeMarker as u8 {
                    continue;
                }
            }

            if oracle != ours {
                println!("{:?} {:?}", oracle, ours);
            }
            assert_eq!(oracle, ours);
        }

        // Empty case.
        let unwind_info = Vec::new();
        let pc = 0;

        let oracle = oracle_binary_search(&unwind_info, pc);
        let ours = our_binary_search(&unwind_info, pc);
        assert_eq!(oracle, ours);
    }

    fn search_in_shards(
        pc: u64,
        shards: &HashMap<u64, Vec<stack_unwind_row_t>>,
        chunks: &Vec<chunk_info_t>,
    ) -> Option<stack_unwind_row_t> {
        let mut found_chunk: Option<chunk_info_t> = None;

        for chunk in chunks {
            // inclusive range [low_pc, high_pc]
            if chunk.low_pc <= pc && pc <= chunk.high_pc {
                found_chunk = Some(*chunk);
                break;
            }
        }

        if let Some(le_chunk) = found_chunk {
            let shard: &Vec<_> = shards.get(&le_chunk.shard_index).unwrap();
            let s: Vec<_> =
                shard[(le_chunk.low_index as usize)..(le_chunk.high_index as usize)].to_vec();
            return our_binary_search(&s, pc);
        }

        None
    }

    #[test]
    fn test_sharding_and_chunking_chunked_between_frames() {
        let mut unwind_info: Vec<_> = vec![
            stack_unwind_row_t {
                pc: 10,
                cfa_type: CfaType::StackPointerOffset as u8,
                cfa_offset: 1,
                rbp_type: RbpType::CfaOffset as u8,
                rbp_offset: 0,
            },
            stack_unwind_row_t {
                pc: 20,
                cfa_type: CfaType::StackPointerOffset as u8,
                cfa_offset: 2,
                rbp_type: RbpType::CfaOffset as u8,
                rbp_offset: 0,
            },
            stack_unwind_row_t {
                pc: 30,
                cfa_type: CfaType::EndFdeMarker as u8,
                cfa_offset: 3,
                rbp_type: RbpType::CfaOffset as u8,
                rbp_offset: 0,
            },
            stack_unwind_row_t {
                pc: 40,
                cfa_type: CfaType::StackPointerOffset as u8,
                cfa_offset: 3,
                rbp_type: RbpType::CfaOffset as u8,
                rbp_offset: 0,
            },
            stack_unwind_row_t {
                pc: 43,
                cfa_type: CfaType::EndFdeMarker as u8,
                cfa_offset: 3,
                rbp_type: RbpType::CfaOffset as u8,
                rbp_offset: 0,
            },
        ];

        unwind_info.sort_by(|a, b| {
            let a_pc = a.pc;
            let b_pc = b.pc;
            a_pc.cmp(&b_pc)
        });

        for shard_size in 1..10 {
            test_sharding_and_chunking_impl(&unwind_info, shard_size);
        }
    }

    //#[test]
    fn test_sharding_and_chunking_binary() {
        let mut unwind_info = in_memory_unwind_info("../testdata/vendored/amd64/redpanda").unwrap();
        unwind_info.sort_by(|a, b| {
            let a_pc = a.pc;
            let b_pc = b.pc;
            a_pc.cmp(&b_pc)
        });
        let unwind_info = remove_unnecesary_markers(&unwind_info);
        let unwind_info = remove_redundant(&unwind_info);

        test_sharding_and_chunking_impl(&unwind_info, 305_123);
    }

    fn test_sharding_and_chunking_impl(unwind_info: &Vec<stack_unwind_row_t>, shard_size: usize) {
        assert!(shard_size > 0);
        assert!(unwind_info[unwind_info.len() - 1].cfa_type == CfaType::EndFdeMarker as u8);

        let mut first_pc = unwind_info[0].pc;
        let mut last_pc = unwind_info[unwind_info.len() - 1].pc;

        // Test edge conditions.
        if first_pc >= 1 {
            first_pc -= 2;
        }
        last_pc += 30;

        let mut live_shard: Vec<stack_unwind_row_t> = Vec::new();
        let mut shard_index = 0_u64;

        let mut shards: HashMap<u64, Vec<stack_unwind_row_t>> = HashMap::new();
        let mut chunks: Vec<chunk_info_t> = Vec::new();

        let mut current: &[stack_unwind_row_t];
        let mut rest: &[stack_unwind_row_t] = &unwind_info[..];

        loop {
            if rest.len() == 0 {
                break;
            }

            if live_shard.len() == shard_size {
                // "persist"
                shards.insert(shard_index, live_shard.clone());
                // create new shard
                shard_index += 1;
                // reset live shard
                live_shard = Vec::new();
            }

            let length: usize = std::cmp::min(shard_size.abs_diff(live_shard.len()), rest.len());
            current = &rest[..length];
            rest = &rest[length..];

            let low_index = live_shard.len() as u64;
            live_shard.append(&mut current.to_vec());
            let high_index = live_shard.len() as u64;

            chunks.push(chunk_info_t {
                low_pc: current[0].pc,
                high_pc: if rest.len() > 0 {
                    rest[0].pc - 1
                } else {
                    current[current.len() - 1].pc
                },
                shard_index: shard_index,
                low_index: low_index,
                high_index: high_index,
            });
        }

        // insert current live shard
        shards.insert(shard_index, live_shard.clone());

        for pc in first_pc..last_pc {
            let oracle = oracle_binary_search(&unwind_info, pc);
            let ours = search_in_shards(pc, &shards, &chunks);

            // search_in_shards can tell when a PC is not contained
            // at all, while the binary search method over the whole
            // unwind info will return the last element
            if oracle == Some(unwind_info[unwind_info.len() - 1]) {
                if oracle.unwrap().cfa_type == CfaType::EndFdeMarker as u8 {
                    continue;
                }
            }

            assert_eq!(oracle, ours, "program counter {}", pc);
        }
    }
}
