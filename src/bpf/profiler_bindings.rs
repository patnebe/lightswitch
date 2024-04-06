#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use plain::Plain;

include!(concat!(env!("OUT_DIR"), "/profiler_bindings.rs"));

unsafe impl Plain for stack_count_key_t {}
unsafe impl Plain for native_stack_t {}
unsafe impl Plain for Event {}
unsafe impl Plain for process_info_t {}
unsafe impl Plain for unwind_info_chunks_t {}
//impl Eq for stack_unwind_row_t {}
//impl PartialOrd for stack_unwind_row_t {}
impl PartialEq for stack_unwind_row_t {
    fn eq(&self, other: &Self) -> bool {
        self.pc == other.pc
            && self.cfa_type == other.cfa_type
            && self.cfa_offset == other.cfa_offset
            && self.rbp_offset == other.rbp_offset
            && self.rbp_type == other.rbp_type
    }
}

#[allow(clippy::derivable_impls)]
impl Default for stack_count_key_t {
    fn default() -> Self {
        Self {
            task_id: 0,
            pid: 0,
            tgid: 0,
            user_stack_id: 0,
            kernel_stack_id: 0,
        }
    }
}
