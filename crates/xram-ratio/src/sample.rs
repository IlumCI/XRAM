//! Sampling real pages to compress.
//!
//! Synthetic data tells you nothing useful about a compression tier. The ratio
//! XRAM gets is a property of what is actually in this machine's memory, so the
//! honest measurement reads real anonymous pages out of a running process.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

/// One private anonymous mapping - the kind that would end up in swap, and so
/// the kind XRAM's compressed tier would hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub label: String,
}

impl Region {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
}

/// Parses `/proc/<pid>/maps`, keeping only mappings that can reach swap.
///
/// File-backed mappings are excluded: clean ones are simply dropped and re-read
/// from their file, so they never occupy a swap tier. Special kernel mappings
/// are excluded because reading them is either forbidden or meaningless.
pub fn anon_regions(maps: &str) -> Vec<Region> {
    maps.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Region> {
    let mut f = line.split_whitespace();
    let range = f.next()?;
    let perms = f.next()?;
    let _offset = f.next()?;
    let _dev = f.next()?;
    let _inode = f.next()?;
    let path = f.next().unwrap_or("");

    // Must be readable and private; shared mappings are backed elsewhere.
    if !perms.starts_with('r') || !perms.ends_with('p') {
        return None;
    }
    // Anonymous means no path, or one of the kernel's anonymous pseudo-regions.
    let anonymous = path.is_empty() || path == "[heap]" || path == "[stack]";
    if !anonymous {
        return None;
    }

    let (s, e) = range.split_once('-')?;
    let start = u64::from_str_radix(s, 16).ok()?;
    let end = u64::from_str_radix(e, 16).ok()?;
    (end > start).then(|| Region {
        start,
        end,
        label: if path.is_empty() {
            "anon".to_owned()
        } else {
            path.to_owned()
        },
    })
}

/// Reads pages from a process's address space, skipping any that are not
/// resident.
///
/// A hole in `/proc/pid/mem` is not an error: an untouched page of a mapping has
/// no backing frame, and reading it fails with EIO. Those pages are exactly the
/// ones that would never reach a swap tier either, so they are skipped rather
/// than counted as zeroes - counting them would inflate the same-fill rate and
/// overstate the ratio.
pub struct MemReader {
    mem: File,
}

impl MemReader {
    pub fn open(pid: u32) -> io::Result<Self> {
        Ok(Self {
            mem: File::open(format!("/proc/{pid}/mem"))?,
        })
    }

    /// Returns `Ok(false)` when the page is not resident.
    pub fn read_page(&self, addr: u64, buf: &mut [u8]) -> io::Result<bool> {
        match self.mem.read_exact_at(buf, addr) {
            Ok(()) => Ok(true),
            Err(e) if matches!(e.raw_os_error(), Some(libc_eio) if libc_eio == 5) => Ok(false),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
55d3f0a00000-55d3f0a21000 r--p 00000000 fd:01 1234    /usr/bin/bash
55d3f0c00000-55d3f0c22000 rw-p 00000000 00:00 0       [heap]
7f2c00000000-7f2c00021000 rw-p 00000000 00:00 0 
7f2c00021000-7f2c00030000 ---p 00000000 00:00 0 
7f2c40000000-7f2c40010000 rw-s 00000000 00:05 99      /dev/shm/thing
7ffd1a000000-7ffd1a021000 rw-p 00000000 00:00 0       [stack]
7ffd1a1fe000-7ffd1a200000 r--p 00000000 00:00 0       [vvar]
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0 [vsyscall]
";

    #[test]
    fn keeps_only_private_anonymous_readable_mappings() {
        let r = anon_regions(MAPS);
        let labels: Vec<_> = r.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, ["[heap]", "anon", "[stack]"]);
    }

    #[test]
    fn excludes_file_backed_mappings() {
        // Clean file pages are dropped and re-read, never swapped, so counting
        // them would measure the wrong population entirely.
        assert!(!anon_regions(MAPS).iter().any(|r| r.label.contains("bash")));
    }

    #[test]
    fn excludes_shared_and_unreadable_mappings() {
        let r = anon_regions(MAPS);
        assert!(!r.iter().any(|x| x.label.contains("shm")), "rw-s is shared");
        // The ---p guard page and the vsyscall page are both unreadable.
        assert!(!r.iter().any(|x| x.start == 0x7f2c00021000));
        assert!(!r.iter().any(|x| x.label.contains("vsyscall")));
    }

    #[test]
    fn excludes_vvar_which_is_file_backed_special() {
        assert!(!anon_regions(MAPS).iter().any(|x| x.label.contains("vvar")));
    }

    #[test]
    fn parses_ranges_correctly() {
        let r = anon_regions(MAPS);
        let heap = &r[0];
        assert_eq!(heap.start, 0x55d3f0c00000);
        assert_eq!(heap.end, 0x55d3f0c22000);
        assert_eq!(heap.len(), 0x22000);
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        assert!(anon_regions("garbage\n\n55d3-\n").is_empty());
    }

    #[test]
    fn zero_length_regions_are_rejected() {
        assert!(anon_regions("1000-1000 rw-p 0 00:00 0 \n").is_empty());
    }
}
