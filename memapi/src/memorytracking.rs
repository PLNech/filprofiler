use im::hashmap as imhashmap;
use inferno::flamegraph;
use itertools::Itertools;
use libc;
use rustc_hash::FxHashMap as HashMap;
use smallstr::SmallString;
use std::cell::RefCell;
use std::collections;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

type CallSiteId = u32;

/// The current Python callstack. We use IDs instead of CallSite objects for
/// performance reasons.
#[derive(Clone, Debug, PartialEq)]
struct Callstack {
    calls: Vec<CallSiteId>,
}

impl Callstack {
    fn new() -> Callstack {
        Callstack { calls: Vec::new() }
    }

    fn start_call(&mut self, function_id: CallSiteId) {
        self.calls.push(function_id);
    }

    fn finish_call(&mut self) {
        self.calls.pop();
    }

    fn as_string(&self, id_to_callsite: &HashMap<CallSiteId, CallSite>) -> String {
        if self.calls.is_empty() {
            "[No Python stack]".to_string()
        } else {
            self.calls
                .iter()
                .map(|id| id_to_callsite.get(id).unwrap())
                .join(";")
        }
    }
}

thread_local!(static THREAD_CALLSTACK: RefCell<Callstack> = RefCell::new(Callstack::new()));

/// A particular place where a call happened.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CallSite {
    module_name: SmallString<[u8; 24]>,
    function_name: SmallString<[u8; 24]>,
}

impl CallSite {
    pub fn new(module_name: &str, function_name: &str) -> CallSite {
        CallSite {
            module_name: SmallString::from_str(module_name),
            function_name: SmallString::from_str(function_name),
        }
    }
}

impl fmt::Display for CallSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.module_name, self.function_name)
    }
}

/// Maps CallSites to integer identifiers used in CallStacks.
struct CallSites {
    max_id: u32,
    callsite_to_id: HashMap<CallSite, CallSiteId>,
}

impl CallSites {
    fn new() -> CallSites {
        CallSites {
            max_id: 0,
            callsite_to_id: HashMap::default(),
        }
    }

    /// Add a (possibly) new CallSite, returning its ID.
    fn get_or_insert_id(&mut self, call_site: CallSite) -> CallSiteId {
        let max_id = &mut self.max_id;
        let result = self.callsite_to_id.entry(call_site).or_insert_with(|| {
            let result = *max_id;
            *max_id += 1;
            result
        });
        *result
    }

    /// Get map from IDs to CallSites.
    fn get_reverse_map(&self) -> HashMap<CallSiteId, CallSite> {
        let mut result = HashMap::default();
        for (call_site, csid) in &(self.callsite_to_id) {
            result.insert(*csid, call_site.clone());
        }
        result
    }
}

/// A specific call to malloc()/calloc().
#[derive(Clone, Debug, PartialEq)]
struct Allocation {
    callstack: Callstack,
    size: libc::size_t,
}

/// The main data structure tracking everything.
struct AllocationTracker {
    current_allocations: imhashmap::HashMap<usize, Allocation>,
    peak_allocations: imhashmap::HashMap<usize, Allocation>,
    current_allocated_bytes: usize,
    peak_allocated_bytes: usize,
    call_sites: CallSites,
}

impl<'a> AllocationTracker {
    fn new() -> AllocationTracker {
        AllocationTracker {
            current_allocations: imhashmap::HashMap::default(),
            peak_allocations: imhashmap::HashMap::default(),
            current_allocated_bytes: 0,
            peak_allocated_bytes: 0,
            call_sites: CallSites::new(),
        }
    }

    /// Add a new allocation based off the current callstack.
    fn add_allocation(&mut self, address: usize, size: libc::size_t, callstack: Callstack) {
        let alloc = Allocation { callstack, size };
        self.current_allocations.insert(address, alloc);
        self.current_allocated_bytes += size;
        if self.current_allocated_bytes > self.peak_allocated_bytes {
            self.peak_allocated_bytes = self.current_allocated_bytes;
            self.peak_allocations = self.current_allocations.clone();
        }
    }

    /// Free an existing allocation.
    fn free_allocation(&mut self, address: usize) {
        // Possibly this allocation doesn't exist; that's OK!
        if let Some(removed) = self.current_allocations.remove(&address) {
            if removed.size > self.current_allocated_bytes {
                // In theory this should never happen, but just in case...
                self.current_allocated_bytes = 0;
            } else {
                self.current_allocated_bytes -= removed.size;
            }
        }
    }

    /// Combine Callstacks and make them human-readable. Duplicate callstacks
    /// have their allocated memory summed.
    fn combine_callstacks(&self) -> collections::HashMap<String, usize> {
        let mut by_call: collections::HashMap<String, usize> = collections::HashMap::new();
        let peak_allocations = &self.peak_allocations;
        let id_to_callsite = self.call_sites.get_reverse_map();
        for Allocation { callstack, size } in peak_allocations.values() {
            let callstack = callstack.as_string(&id_to_callsite);
            let entry = by_call.entry(callstack).or_insert(0);
            *entry += size;
        }
        by_call
    }

    /// Dump all callstacks in peak memory usage to various files describing the
    /// memory usage.
    fn dump_peak_to_flamegraph(&self, path: &str) {
        eprintln!("=fil-profile= Preparing to write to {}", path);
        let directory_path = Path::new(path);
        if !directory_path.exists() {
            fs::create_dir(directory_path)
                .expect("=fil-profile= Couldn't create the output directory.");
        } else if !directory_path.is_dir() {
            panic!("=fil-profile= Output path must be a directory.");
        }
        let by_call = self.combine_callstacks();
        let lines: Vec<String> = by_call
            .iter()
            .map(|(callstack, size)| format!("{} {}", callstack, *size))
            .collect();
        let raw_path = directory_path
            .join("peak-memory.prof")
            .to_str()
            .unwrap()
            .to_string();
        if let Err(e) = write_lines(&lines, &raw_path) {
            eprintln!("=fil-profile= Error writing raw profiling data: {}", e);
        }
        let svg_path = directory_path
            .join("peak-memory.svg")
            .to_str()
            .unwrap()
            .to_string();
        match write_flamegraph(
            lines.iter().map(|s| s.as_ref()),
            &svg_path,
            self.peak_allocated_bytes,
            false,
        ) {
            Ok(_) => {
                eprintln!(
                    "=fil-profile= Wrote memory usage flamegraph to {}",
                    svg_path
                );
            }
            Err(e) => {
                eprintln!("=fil-profile= Error writing SVG: {}", e);
            }
        }
        let svg_path = directory_path
            .join("peak-memory-reversed.svg")
            .to_str()
            .unwrap()
            .to_string();
        match write_flamegraph(
            lines.iter().map(|s| s.as_ref()),
            &svg_path,
            self.peak_allocated_bytes,
            true,
        ) {
            Ok(_) => {
                eprintln!(
                    "=fil-profile= Wrote memory usage flamegraph to {}",
                    svg_path
                );
            }
            Err(e) => {
                eprintln!("=fil-profile= Error writing SVG: {}", e);
            }
        }
    }
}

lazy_static! {
    static ref ALLOCATIONS: Mutex<AllocationTracker> = Mutex::new(AllocationTracker::new());
}

/// Add to per-thread function stack:
pub fn start_call(call_site: CallSite) {
    let mut allocations = ALLOCATIONS.lock().unwrap();
    let id = allocations.call_sites.get_or_insert_id(call_site);
    THREAD_CALLSTACK.with(|cs| {
        cs.borrow_mut().start_call(id);
    });
}

/// Finish off (and move to reporting structure) current function in function
/// stack.
pub fn finish_call() {
    THREAD_CALLSTACK.with(|cs| {
        cs.borrow_mut().finish_call();
    });
}

/// Add a new allocation based off the current callstack.
pub fn add_allocation(address: usize, size: libc::size_t) {
    let callstack: Callstack = THREAD_CALLSTACK.with(|cs| (*cs.borrow()).clone());
    let mut allocations = ALLOCATIONS.lock().unwrap();
    allocations.add_allocation(address, size, callstack);
}

/// Free an existing allocation.
pub fn free_allocation(address: usize) {
    let mut allocations = ALLOCATIONS.lock().unwrap();
    allocations.free_allocation(address);
}

/// Reset internal state.
pub fn reset() {
    *ALLOCATIONS.lock().unwrap() = AllocationTracker::new();
}

/// Dump all callstacks in peak memory usage to format used by flamegraph.
pub fn dump_peak_to_flamegraph(path: &str) {
    let allocations = &ALLOCATIONS.lock().unwrap();
    allocations.dump_peak_to_flamegraph(path);
}

/// Write strings to disk, one line per string.
fn write_lines(lines: &Vec<String>, path: &str) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    for line in lines.iter() {
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

/// Write a flamegraph SVG to disk, given lines in summarized format.
fn write_flamegraph<'a, I: IntoIterator<Item = &'a str>>(
    lines: I,
    path: &str,
    peak_bytes: usize,
    reversed: bool,
) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let title = format!(
        "Peak Tracked Memory Usage{} ({:.1} MiB)",
        if reversed { ", Reversed" } else { "" },
        peak_bytes as f64 / (1024.0 * 1024.0)
    );
    let mut options = flamegraph::Options {
        title,
        direction: flamegraph::Direction::Inverted,
        count_name: "bytes".to_string(),
        font_size: 16,
        font_type: "mono".to_string(),
        frame_height: 22,
        subtitle: Some("SUBTITLE-HERE".to_string()),
        reverse_stack_order: reversed,
        ..Default::default()
    };
    if let Err(e) = flamegraph::from_lines(&mut options, lines, file) {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("{}", e),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AllocationTracker, CallSite, CallSites, Callstack};
    use itertools::Itertools;
    use proptest::prelude::*;
    use std::collections;

    proptest! {
        #[test]
        fn current_allocated_matches_sum_of_allocations(
            // Allocated bytes. Will use index as the memory address.
            allocated_sizes in prop::collection::vec(1..1000 as usize, 10..20),
            // Allocations to free.
            free_indices in prop::collection::vec(any::<prop::sample::Index>(), 5..10)
        ) {
            let mut tracker = AllocationTracker::new();
            for i in 0..allocated_sizes.len() {
                tracker.add_allocation(i as usize,*allocated_sizes.get(i).unwrap(), Callstack::new());
            }
            let mut expected_sum = allocated_sizes.iter().sum();
            prop_assert_eq!(tracker.current_allocated_bytes, expected_sum);
            for i in free_indices.iter().map(|i|i.index(allocated_sizes.len())).unique() {
                expected_sum -= allocated_sizes.get(i).unwrap();
                tracker.free_allocation(i);
                prop_assert_eq!(tracker.current_allocated_bytes, expected_sum);
            }
        }
    }

    #[test]
    fn peak_allocations_only_updated_on_new_peaks() {
        let mut tracker = AllocationTracker::new();
        let mut cs1 = Callstack::new();
        cs1.start_call(1);
        let mut cs2 = Callstack::new();
        cs2.start_call(2);

        tracker.add_allocation(1, 1000, cs1.clone());
        // Peak should now match current allocations:
        assert_eq!(tracker.current_allocations, tracker.peak_allocations);
        assert_eq!(tracker.peak_allocated_bytes, 1000);
        let previous_peak = tracker.peak_allocations.clone();

        // Free the allocation:
        tracker.free_allocation(1);
        assert_eq!(tracker.current_allocated_bytes, 0);
        assert_eq!(previous_peak, tracker.peak_allocations);
        assert_eq!(tracker.peak_allocated_bytes, 1000);

        // Add allocation, still less than 1000:
        tracker.add_allocation(3, 123, cs1.clone());
        assert_eq!(previous_peak, tracker.peak_allocations);
        assert_eq!(tracker.peak_allocated_bytes, 1000);

        // Add allocation that goes past previous peak
        tracker.add_allocation(2, 2000, cs2.clone());
        assert_eq!(tracker.current_allocations, tracker.peak_allocations);
        assert_eq!(tracker.peak_allocated_bytes, 2123);
    }

    #[test]
    fn callsites_notices_duplicate_callsites() {
        let callsite1 = CallSite::new("a", "af");
        let callsite2 = CallSite::new("b", "af");
        let callsite3 = CallSite::new("a", "bf");
        let mut callsites = CallSites::new();
        let id1 = callsites.get_or_insert_id(callsite1.clone());
        let id1b = callsites.get_or_insert_id(callsite1);
        let id2 = callsites.get_or_insert_id(callsite2);
        let id3 = callsites.get_or_insert_id(callsite3.clone());
        let id3b = callsites.get_or_insert_id(callsite3.clone());
        assert_eq!(id1, id1b);
        assert_ne!(id1, id2);
        assert_ne!(id1, id3);
        assert_ne!(id2, id3);
        assert_eq!(id3, id3b);
    }

    #[test]
    fn combine_callstacks_and_sum_allocations() {
        let mut tracker = AllocationTracker::new();
        let id1 = tracker
            .call_sites
            .get_or_insert_id(CallSite::new("a", "af"));

        let id2 = tracker
            .call_sites
            .get_or_insert_id(CallSite::new("b", "bf"));

        let id3 = tracker
            .call_sites
            .get_or_insert_id(CallSite::new("c", "cf"));
        let mut cs1 = Callstack::new();
        cs1.start_call(id1);
        cs1.start_call(id2);
        let mut cs2 = Callstack::new();
        cs2.start_call(id3);

        tracker.add_allocation(1, 1000, cs1.clone());
        tracker.add_allocation(2, 234, cs2.clone());
        tracker.add_allocation(3, 50000, cs1.clone());

        let mut expected: collections::HashMap<String, usize> = collections::HashMap::new();
        expected.insert("a:af;b:bf".to_string(), 51000);
        expected.insert("c:cf".to_string(), 234);
        assert_eq!(expected, tracker.combine_callstacks());
    }
}
