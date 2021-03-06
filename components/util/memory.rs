/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Memory profiling functions.

use libc::{c_char,c_int,c_void,size_t};
use std::borrow::ToOwned;
use std::collections::HashMap;
use std::collections::LinkedList as DList;
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::iter::AdditiveIterator;
use std::old_io::timer::sleep;
#[cfg(target_os="linux")]
use std::old_io::File;
use std::mem::{size_of, transmute};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel, Receiver};
use std::time::duration::Duration;
use task::spawn_named;
#[cfg(target_os="macos")]
use task_info::task_basic_info::{virtual_size,resident_size};

extern {
    // Get the size of a heap block.
    //
    // Ideally Rust would expose a function like this in std::rt::heap, which would avoid the
    // jemalloc dependence.
    //
    // The C prototype is `je_malloc_usable_size(JEMALLOC_USABLE_SIZE_CONST void *ptr)`. On some
    // platforms `JEMALLOC_USABLE_SIZE_CONST` is `const` and on some it is empty. But in practice
    // this function doesn't modify the contents of the block that `ptr` points to, so we use
    // `*const c_void` here.
    fn je_malloc_usable_size(ptr: *const c_void) -> size_t;
}

// A wrapper for je_malloc_usable_size that handles `EMPTY` and returns `usize`.
pub fn heap_size_of(ptr: *const c_void) -> usize {
    if ptr == ::std::rt::heap::EMPTY as *const c_void {
        0
    } else {
        unsafe { je_malloc_usable_size(ptr) as usize }
    }
}

// The simplest trait for measuring the size of heap data structures. More complex traits that
// return multiple measurements -- e.g. measure text separately from images -- are also possible,
// and should be used when appropriate.
//
// FIXME(njn): it would be nice to be able to derive this trait automatically, given that
// implementations are mostly repetitive and mechanical.
//
pub trait SizeOf {
    /// Measure the size of any heap-allocated structures that hang off this value, but not the
    /// space taken up by the value itself (i.e. what size_of::<T> measures, more or less); that
    /// space is handled by the implementation of SizeOf for Box<T> below.
    fn size_of_excluding_self(&self) -> usize;
}

// There are two possible ways to measure the size of `self` when it's on the heap: compute it
// (with `::std::rt::heap::usable_size(::std::mem::size_of::<T>(), 0)`) or measure it directly
// using the heap allocator (with `heap_size_of`). We do the latter, for the following reasons.
//
// * The heap allocator is the true authority for the sizes of heap blocks; its measurement is
//   guaranteed to be correct. In comparison, size computations are error-prone. (For example, the
//   `rt::heap::usable_size` function used in some of Rust's non-default allocator implementations
//   underestimate the true usable size of heap blocks, which is safe in general but would cause
//   under-measurement here.)
//
// * If we measure something that isn't a heap block, we'll get a crash. This keeps us honest,
//   which is important because unsafe code is involved and this can be gotten wrong.
//
// However, in the best case, the two approaches should give the same results.
//
impl<T: SizeOf> SizeOf for Box<T> {
    fn size_of_excluding_self(&self) -> usize {
        // Measure size of `self`.
        heap_size_of(&**self as *const T as *const c_void) + (**self).size_of_excluding_self()
    }
}

impl SizeOf for String {
    fn size_of_excluding_self(&self) -> usize {
        heap_size_of(self.as_ptr() as *const c_void)
    }
}

impl<T: SizeOf> SizeOf for Option<T> {
    fn size_of_excluding_self(&self) -> usize {
        match *self {
            None => 0,
            Some(ref x) => x.size_of_excluding_self()
        }
    }
}

impl<T: SizeOf> SizeOf for Arc<T> {
    fn size_of_excluding_self(&self) -> usize {
        (**self).size_of_excluding_self()
    }
}

impl<T: SizeOf> SizeOf for Vec<T> {
    fn size_of_excluding_self(&self) -> usize {
        heap_size_of(self.as_ptr() as *const c_void) +
            self.iter().fold(0, |n, elem| n + elem.size_of_excluding_self())
    }
}

// FIXME(njn): We can't implement SizeOf accurately for DList because it requires access to the
// private Node type. Eventually we'll want to add SizeOf (or equivalent) to Rust itself. In the
// meantime, we use the dirty hack of transmuting DList into an identical type (DList2) and
// measuring that.
impl<T: SizeOf> SizeOf for DList<T> {
    fn size_of_excluding_self(&self) -> usize {
        let list2: &DList2<T> = unsafe { transmute(self) };
        list2.size_of_excluding_self()
    }
}

struct DList2<T> {
    _length: usize,
    list_head: Link<T>,
    _list_tail: Rawlink<Node<T>>,
}

type Link<T> = Option<Box<Node<T>>>;

struct Rawlink<T> {
    _p: *mut T,
}

struct Node<T> {
    next: Link<T>,
    _prev: Rawlink<Node<T>>,
    value: T,
}

impl<T: SizeOf> SizeOf for Node<T> {
    // Unlike most size_of_excluding_self() functions, this one does *not* measure descendents.
    // Instead, DList2<T>::size_of_excluding_self() handles that, so that it can use iteration
    // instead of recursion, which avoids potentially blowing the stack.
    fn size_of_excluding_self(&self) -> usize {
        self.value.size_of_excluding_self()
    }
}

impl<T: SizeOf> SizeOf for DList2<T> {
    fn size_of_excluding_self(&self) -> usize {
        let mut size = 0;
        let mut curr: &Link<T> = &self.list_head;
        while curr.is_some() {
            size += (*curr).size_of_excluding_self();
            curr = &curr.as_ref().unwrap().next;
        }
        size
    }
}

// This is a basic sanity check. If the representation of DList changes such that it becomes a
// different size to DList2, this will fail at compile-time.
#[allow(dead_code)]
unsafe fn dlist2_check() {
    transmute::<DList<i32>, DList2<i32>>(panic!());
}

// Currently, types that implement the Drop type are larger than those that don't. Because DList
// implements Drop, DList2 must also so that dlist2_check() doesn't fail.
#[unsafe_destructor]
impl<T> Drop for DList2<T> {
    fn drop(&mut self) {}
}

//---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MemoryProfilerChan(pub Sender<MemoryProfilerMsg>);

impl MemoryProfilerChan {
    pub fn send(&self, msg: MemoryProfilerMsg) {
        let MemoryProfilerChan(ref c) = *self;
        c.send(msg).unwrap();
    }
}

pub struct MemoryReport {
    /// The identifying name for this report.
    pub name: String,

    /// The size, in bytes.
    pub size: u64,
}

/// A channel through which memory reports can be sent.
#[derive(Clone)]
pub struct MemoryReportsChan(pub Sender<Vec<MemoryReport>>);

impl MemoryReportsChan {
    pub fn send(&self, report: Vec<MemoryReport>) {
        let MemoryReportsChan(ref c) = *self;
        c.send(report).unwrap();
    }
}

/// A memory reporter is capable of measuring some data structure of interest. Because it needs
/// to be passed to and registered with the MemoryProfiler, it's typically a "small" (i.e. easily
/// cloneable) value that provides access to a "large" data structure, e.g. a channel that can
/// inject a request for measurements into the event queue associated with the "large" data
/// structure.
pub trait MemoryReporter {
    /// Collect one or more memory reports. Returns true on success, and false on failure.
    fn collect_reports(&self, reports_chan: MemoryReportsChan) -> bool;
}

/// Messages that can be sent to the memory profiler thread.
pub enum MemoryProfilerMsg {
    /// Register a MemoryReporter with the memory profiler. The String is only used to identify the
    /// reporter so it can be unregistered later. The String must be distinct from that used by any
    /// other registered reporter otherwise a panic will occur.
    RegisterMemoryReporter(String, Box<MemoryReporter + Send>),

    /// Unregister a MemoryReporter with the memory profiler. The String must match the name given
    /// when the reporter was registered. If the String does not match the name of a registered
    /// reporter a panic will occur.
    UnregisterMemoryReporter(String),

    /// Triggers printing of the memory profiling metrics.
    Print,

    /// Tells the memory profiler to shut down.
    Exit,
}

pub struct MemoryProfiler {
    /// The port through which messages are received.
    pub port: Receiver<MemoryProfilerMsg>,

    /// Registered memory reporters.
    reporters: HashMap<String, Box<MemoryReporter + Send>>,
}

impl MemoryProfiler {
    pub fn create(period: Option<f64>) -> MemoryProfilerChan {
        let (chan, port) = channel();

        // Create the timer thread if a period was provided.
        if let Some(period) = period {
            let period_ms = Duration::milliseconds((period * 1000f64) as i64);
            let chan = chan.clone();
            spawn_named("Memory profiler timer".to_owned(), move || {
                loop {
                    sleep(period_ms);
                    if chan.send(MemoryProfilerMsg::Print).is_err() {
                        break;
                    }
                }
            });
        }

        // Always spawn the memory profiler. If there is no timer thread it won't receive regular
        // `Print` events, but it will still receive the other events.
        spawn_named("Memory profiler".to_owned(), move || {
            let mut memory_profiler = MemoryProfiler::new(port);
            memory_profiler.start();
        });

        let memory_profiler_chan = MemoryProfilerChan(chan);

        // Register the system memory reporter, which will run on the memory profiler's own thread.
        // It never needs to be unregistered, because as long as the memory profiler is running the
        // system memory reporter can make measurements.
        let system_reporter = Box::new(SystemMemoryReporter);
        memory_profiler_chan.send(MemoryProfilerMsg::RegisterMemoryReporter("system".to_owned(),
                                                                            system_reporter));

        memory_profiler_chan
    }

    pub fn new(port: Receiver<MemoryProfilerMsg>) -> MemoryProfiler {
        MemoryProfiler {
            port: port,
            reporters: HashMap::new(),
        }
    }

    pub fn start(&mut self) {
        loop {
            match self.port.recv() {
               Ok(msg) => {
                   if !self.handle_msg(msg) {
                       break
                   }
               }
               _ => break
            }
        }
    }

    fn handle_msg(&mut self, msg: MemoryProfilerMsg) -> bool {
        match msg {
            MemoryProfilerMsg::RegisterMemoryReporter(name, reporter) => {
                // Panic if it has already been registered.
                let name_clone = name.clone();
                match self.reporters.insert(name, reporter) {
                    None => true,
                    Some(_) =>
                        panic!(format!("RegisterMemoryReporter: '{}' name is already in use",
                                       name_clone)),
                }
            },

            MemoryProfilerMsg::UnregisterMemoryReporter(name) => {
                // Panic if it hasn't previously been registered.
                match self.reporters.remove(&name) {
                    Some(_) => true,
                    None =>
                        panic!(format!("UnregisterMemoryReporter: '{}' name is unknown", &name)),
                }
            },

            MemoryProfilerMsg::Print => {
                self.handle_print_msg();
                true
            },

            MemoryProfilerMsg::Exit => false
        }
    }

    fn handle_print_msg(&self) {
        println!("{:12}: {}", "_size (MiB)_", "_category_");

        // Collect reports from memory reporters.
        //
        // This serializes the report-gathering. It might be worth creating a new scoped thread for
        // each reporter once we have enough of them.
        //
        // If anything goes wrong with a reporter, we just skip it.
        for reporter in self.reporters.values() {
            let (chan, port) = channel();
            if reporter.collect_reports(MemoryReportsChan(chan)) {
                if let Ok(reports) = port.recv() {
                    for report in reports {
                        let mebi = 1024f64 * 1024f64;
                        println!("{:12.2}: {}", (report.size as f64) / mebi, report.name);
                    }
                }
            }
        }

        println!("");
    }
}

/// Collects global measurements from the OS and heap allocators.
struct SystemMemoryReporter;

impl MemoryReporter for SystemMemoryReporter {
    fn collect_reports(&self, reports_chan: MemoryReportsChan) -> bool {
        let mut reports = vec![];
        {
            let mut report = |name: &str, size| {
                if let Some(size) = size {
                    reports.push(MemoryReport { name: name.to_owned(), size: size });
                }
            };

            // Virtual and physical memory usage, as reported by the OS.
            report("vsize", get_vsize());
            report("resident", get_resident());

            // Memory segments, as reported by the OS.
            for seg in get_resident_segments().iter() {
                report(seg.0.as_slice(), Some(seg.1));
            }

            // Total number of bytes allocated by the application on the system
            // heap.
            report("system-heap-allocated", get_system_heap_allocated());

            // The descriptions of the following jemalloc measurements are taken
            // directly from the jemalloc documentation.

            // "Total number of bytes allocated by the application."
            report("jemalloc-heap-allocated", get_jemalloc_stat("stats.allocated"));

            // "Total number of bytes in active pages allocated by the application.
            // This is a multiple of the page size, and greater than or equal to
            // |stats.allocated|."
            report("jemalloc-heap-active", get_jemalloc_stat("stats.active"));

            // "Total number of bytes in chunks mapped on behalf of the application.
            // This is a multiple of the chunk size, and is at least as large as
            // |stats.active|. This does not include inactive chunks."
            report("jemalloc-heap-mapped", get_jemalloc_stat("stats.mapped"));
        }
        reports_chan.send(reports);

        true
    }
}


#[cfg(target_os="linux")]
extern {
    fn mallinfo() -> struct_mallinfo;
}

#[cfg(target_os="linux")]
#[repr(C)]
pub struct struct_mallinfo {
    arena:    c_int,
    ordblks:  c_int,
    smblks:   c_int,
    hblks:    c_int,
    hblkhd:   c_int,
    usmblks:  c_int,
    fsmblks:  c_int,
    uordblks: c_int,
    fordblks: c_int,
    keepcost: c_int,
}

#[cfg(target_os="linux")]
fn get_system_heap_allocated() -> Option<u64> {
    let mut info: struct_mallinfo;
    unsafe {
        info = mallinfo();
    }
    // The documentation in the glibc man page makes it sound like |uordblks|
    // would suffice, but that only gets the small allocations that are put in
    // the brk heap. We need |hblkhd| as well to get the larger allocations
    // that are mmapped.
    Some((info.hblkhd + info.uordblks) as u64)
}

#[cfg(not(target_os="linux"))]
fn get_system_heap_allocated() -> Option<u64> {
    None
}

extern {
    fn je_mallctl(name: *const c_char, oldp: *mut c_void, oldlenp: *mut size_t,
                  newp: *mut c_void, newlen: size_t) -> c_int;
}

fn get_jemalloc_stat(value_name: &str) -> Option<u64> {
    // Before we request the measurement of interest, we first send an "epoch"
    // request. Without that jemalloc gives cached statistics(!) which can be
    // highly inaccurate.
    let epoch_name = "epoch";
    let epoch_c_name = CString::from_slice(epoch_name.as_bytes());
    let mut epoch: u64 = 0;
    let epoch_ptr = &mut epoch as *mut _ as *mut c_void;
    let mut epoch_len = size_of::<u64>() as size_t;

    let value_c_name = CString::from_slice(value_name.as_bytes());
    let mut value: size_t = 0;
    let value_ptr = &mut value as *mut _ as *mut c_void;
    let mut value_len = size_of::<size_t>() as size_t;

    // Using the same values for the `old` and `new` parameters is enough
    // to get the statistics updated.
    let rv = unsafe {
        je_mallctl(epoch_c_name.as_ptr(), epoch_ptr, &mut epoch_len, epoch_ptr,
                   epoch_len)
    };
    if rv != 0 {
        return None;
    }

    let rv = unsafe {
        je_mallctl(value_c_name.as_ptr(), value_ptr, &mut value_len,
                   null_mut(), 0)
    };
    if rv != 0 {
        return None;
    }

    Some(value as u64)
}

// Like std::macros::try!, but for Option<>.
macro_rules! option_try(
    ($e:expr) => (match $e { Some(e) => e, None => return None })
);

#[cfg(target_os="linux")]
fn get_proc_self_statm_field(field: usize) -> Option<u64> {
    let mut f = File::open(&Path::new("/proc/self/statm"));
    match f.read_to_string() {
        Ok(contents) => {
            let s = option_try!(contents.as_slice().words().nth(field));
            let npages = option_try!(s.parse::<u64>().ok());
            Some(npages * (::std::env::page_size() as u64))
        }
        Err(_) => None
    }
}

#[cfg(target_os="linux")]
fn get_vsize() -> Option<u64> {
    get_proc_self_statm_field(0)
}

#[cfg(target_os="linux")]
fn get_resident() -> Option<u64> {
    get_proc_self_statm_field(1)
}

#[cfg(target_os="macos")]
fn get_vsize() -> Option<u64> {
    virtual_size()
}

#[cfg(target_os="macos")]
fn get_resident() -> Option<u64> {
    resident_size()
}

#[cfg(not(any(target_os="linux", target_os = "macos")))]
fn get_vsize() -> Option<u64> {
    None
}

#[cfg(not(any(target_os="linux", target_os = "macos")))]
fn get_resident() -> Option<u64> {
    None
}

#[cfg(target_os="linux")]
fn get_resident_segments() -> Vec<(String, u64)> {
    use regex::Regex;
    use std::collections::HashMap;
    use std::collections::hash_map::Entry;

    // The first line of an entry in /proc/<pid>/smaps looks just like an entry
    // in /proc/<pid>/maps:
    //
    //   address           perms offset  dev   inode  pathname
    //   02366000-025d8000 rw-p 00000000 00:00 0      [heap]
    //
    // Each of the following lines contains a key and a value, separated
    // by ": ", where the key does not contain either of those characters.
    // For example:
    //
    //   Rss:           132 kB

    let path = Path::new("/proc/self/smaps");
    let mut f = ::std::old_io::BufferedReader::new(File::open(&path));

    let seg_re = Regex::new(
        r"^[:xdigit:]+-[:xdigit:]+ (....) [:xdigit:]+ [:xdigit:]+:[:xdigit:]+ \d+ +(.*)").unwrap();
    let rss_re = Regex::new(r"^Rss: +(\d+) kB").unwrap();

    // We record each segment's resident size.
    let mut seg_map: HashMap<String, u64> = HashMap::new();

    #[derive(PartialEq)]
    enum LookingFor { Segment, Rss }
    let mut looking_for = LookingFor::Segment;

    let mut curr_seg_name = String::new();

    // Parse the file.
    for line in f.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        if looking_for == LookingFor::Segment {
            // Look for a segment info line.
            let cap = match seg_re.captures(line.as_slice()) {
                Some(cap) => cap,
                None => continue,
            };
            let perms = cap.at(1).unwrap();
            let pathname = cap.at(2).unwrap();

            // Construct the segment name from its pathname and permissions.
            curr_seg_name.clear();
            curr_seg_name.push_str("- ");
            if pathname == "" || pathname.starts_with("[stack:") {
                // Anonymous memory. Entries marked with "[stack:nnn]"
                // look like thread stacks but they may include other
                // anonymous mappings, so we can't trust them and just
                // treat them as entirely anonymous.
                curr_seg_name.push_str("anonymous");
            } else {
                curr_seg_name.push_str(pathname);
            }
            curr_seg_name.push_str(" (");
            curr_seg_name.push_str(perms);
            curr_seg_name.push_str(")");

            looking_for = LookingFor::Rss;
        } else {
            // Look for an "Rss:" line.
            let cap = match rss_re.captures(line.as_slice()) {
                Some(cap) => cap,
                None => continue,
            };
            let rss = cap.at(1).unwrap().parse::<u64>().unwrap() * 1024;

            if rss > 0 {
                // Aggregate small segments into "- other".
                let seg_name = if rss < 512 * 1024 {
                    "- other".to_owned()
                } else {
                    curr_seg_name.clone()
                };
                match seg_map.entry(seg_name) {
                    Entry::Vacant(entry) => { entry.insert(rss); },
                    Entry::Occupied(mut entry) => *entry.get_mut() += rss,
                }
            }

            looking_for = LookingFor::Segment;
        }
    }

    let mut segs: Vec<(String, u64)> = seg_map.into_iter().collect();

    // Get the total and add it to the vector. Note that this total differs
    // from the "resident" measurement obtained via /proc/<pid>/statm in
    // get_resident(). It's unclear why this difference occurs; for some
    // processes the measurements match, but for Servo they do not.
    let total = segs.iter().map(|&(_, size)| size).sum();
    segs.push(("resident-according-to-smaps".to_owned(), total));

    // Sort by size; the total will be first.
    segs.sort_by(|&(_, rss1), &(_, rss2)| rss2.cmp(&rss1));

    segs
}

#[cfg(not(target_os="linux"))]
fn get_resident_segments() -> Vec<(String, u64)> {
    vec![]
}

