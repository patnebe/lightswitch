use std::{fs::File, io::Read};

use anyhow::{Context, Error};

#[derive(Debug, PartialEq)]
pub struct AddressBlockRange {
    pub addr: u64,
    pub prefix_len: u32,
}

/// Calculate addresses for longest prefix match.
///
/// For a given address range, calculate all the prefix ranges to ensure searching
/// with Longest Prefix Match returns the precise value we want. This is typically
/// used in networking to select the right subnet.
pub fn summarize_address_range(low: u64, high: u64) -> Vec<AddressBlockRange> {
    let mut res = Vec::new();
    let mut curr = low;

    if low > high {
        panic!("first {:x} > last {:x}", low, high);
    }

    while curr <= high {
        let number_of_bits = std::cmp::min(
            curr.trailing_zeros(),
            (64 - (high - curr + 1).leading_zeros()) - 1,
        );
        res.push(AddressBlockRange {
            addr: curr,
            prefix_len: 64 - number_of_bits,
        });
        curr += 1 << number_of_bits;
        if curr - 1 == u64::MAX {
            break;
        }
    }

    res
}

fn _read_cpu_range(path: &str) -> Result<Vec<u32>, Error> {
    let mut cpus: Vec<_> = vec![];
    let mut fh = File::open(path)?;
    let mut cpu_range_str = String::new();
    fh.read_to_string(&mut cpu_range_str)?;

    for cpu_range in cpu_range_str.split(',') {
        let rangeop_result = cpu_range.find('-');
        match rangeop_result {
            None => cpus.push(
                cpu_range
                    .trim_end()
                    .parse::<u32>()
                    .with_context(|| "Failed to parse lone CPU".to_string())?,
            ),
            Some(index) => {
                let start = cpu_range[..index]
                    .trim_end()
                    .parse::<u32>()
                    .with_context(|| "Failed to parse starting CPU".to_string())?;
                let end = cpu_range[index + 1..]
                    .trim_end()
                    .parse::<u32>()
                    .with_context(|| "Failed to parse ending CPU".to_string())?;
                cpus.extend(start..end + 1);
            }
        }
    }

    Ok(cpus)
}

pub fn get_online_cpus() -> Result<Vec<u32>, Error> {
    let cpus: Vec<u32> = _read_cpu_range("/sys/devices/system/cpu/online")?;

    Ok(cpus)
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;
    use tempdir::TempDir;

    use libbpf_rs::libbpf_sys;
    use libbpf_rs::MapCore;
    use libbpf_rs::MapFlags;
    use libbpf_rs::MapHandle;
    use libbpf_rs::MapType;

    use crate::bpf::profiler_bindings::exec_mappings_key;
    use crate::bpf::profiler_bindings::mapping_t;
    use crate::util::*;

    #[test]
    fn test_summarize_address_range() {
        assert_eq!(
            summarize_address_range(0, 100),
            vec![
                AddressBlockRange {
                    addr: 0,
                    prefix_len: 58
                },
                AddressBlockRange {
                    addr: 64,
                    prefix_len: 59
                },
                AddressBlockRange {
                    addr: 96,
                    prefix_len: 62
                },
                AddressBlockRange {
                    addr: 100,
                    prefix_len: 64
                }
            ]
        );
    }

    #[test]
    fn longest_prefix_match_exhaustive_integration_tests() {
        let opts = libbpf_sys::bpf_map_create_opts {
            sz: size_of::<libbpf_sys::bpf_map_create_opts>() as libbpf_sys::size_t,
            map_flags: libbpf_sys::BPF_F_NO_PREALLOC,
            ..Default::default()
        };

        let map = MapHandle::create(
            MapType::LpmTrie,
            Some("lpm_test_map"),
            std::mem::size_of::<exec_mappings_key>() as u32,
            std::mem::size_of::<mapping_t>() as u32,
            1024,
            &opts,
        )
        .unwrap();

        let mapping1 = mapping_t {
            executable_id: 1111,
            load_address: 1111,
            begin: 0x7f7428ea8000,
            end: 0x7f7428f50000,
            type_: 1,
        };

        let mapping2 = mapping_t {
            executable_id: 2222,
            load_address: 2222,
            begin: 0x7f7428f85000,
            end: 0x7f74290e5000,
            type_: 2,
        };

        assert!(mapping1.begin < mapping1.end);
        assert!(mapping2.begin < mapping2.end);
        assert!(mapping1.end < mapping2.begin);

        for address_range in summarize_address_range(mapping1.begin, mapping1.end - 1) {
            let key =
                exec_mappings_key::new(510530, address_range.addr, 32 + address_range.prefix_len);
            map.update(
                unsafe { plain::as_bytes(&key) },
                unsafe { plain::as_bytes(&mapping1) },
                MapFlags::ANY,
            )
            .unwrap();
        }

        for address_range in summarize_address_range(mapping2.begin, mapping2.end - 1) {
            let key =
                exec_mappings_key::new(510530, address_range.addr, 32 + address_range.prefix_len);
            map.update(
                unsafe { plain::as_bytes(&key) },
                unsafe { plain::as_bytes(&mapping2) },
                MapFlags::ANY,
            )
            .unwrap();
        }

        let mut key = exec_mappings_key::new(510530, 0x0, 32 + 64);

        // Test non existent key.
        key.data = (0x0_u64).to_be();
        let retrieved = map
            .lookup(unsafe { plain::as_bytes(&key) }, MapFlags::ANY)
            .unwrap();
        assert_eq!(retrieved, None);

        // First mapping tests.
        for addr in mapping1.begin..mapping1.end {
            key.data = addr.to_be();
            let retrieved = map
                .lookup(unsafe { plain::as_bytes(&key) }, MapFlags::ANY)
                .unwrap()
                .unwrap();
            let parsed: mapping_t = *plain::from_bytes(&retrieved).unwrap();
            assert_eq!(parsed.executable_id, mapping1.executable_id);
        }

        // Second mapping tests.
        for addr in mapping2.begin..mapping2.end {
            key.data = addr.to_be();
            let retrieved = map
                .lookup(unsafe { plain::as_bytes(&key) }, MapFlags::ANY)
                .unwrap()
                .unwrap();
            let parsed: mapping_t = *plain::from_bytes(&retrieved).unwrap();
            assert_eq!(parsed.executable_id, mapping2.executable_id);
        }
    }

    #[test]
    fn cpu_ranges_to_list() {
        use std::io::Seek;
        use std::io::Write;

        let tmp_dir = TempDir::new("cpu_devs").unwrap();
        let file_path = tmp_dir.path().join("online");
        let mut tmp_file = File::create(file_path.clone()).unwrap();
        let file_str = file_path.to_str().unwrap();

        writeln!(tmp_file, "0").unwrap();
        let cpus = _read_cpu_range(file_str).unwrap();
        assert_eq!(cpus, vec![0]);

        tmp_file.rewind().unwrap();
        writeln!(tmp_file, "0-7").unwrap();

        let cpus = _read_cpu_range(file_str).unwrap();
        assert_eq!(cpus, (0..=7).collect::<Vec<_>>());

        tmp_file.rewind().unwrap();
        writeln!(tmp_file, "0-7,16-23").unwrap();

        let cpus = _read_cpu_range(file_str).unwrap();
        let expected = (0..=7).chain(16..=23).collect::<Vec<_>>();

        assert_eq!(cpus, expected);

        tmp_file.rewind().unwrap();
        writeln!(tmp_file, "0-1,3,7-9,48,49").unwrap();

        let cpus = _read_cpu_range(file_str).unwrap();
        assert_eq!(
            cpus,
            (0..=1)
                .chain(3..=3)
                .chain(7..=9)
                .chain(48..=48)
                .chain(49..=49)
                .collect::<Vec<_>>()
        );

        drop(tmp_file);
        tmp_dir.close().expect("tempdir should be destroyed");
    }
}
