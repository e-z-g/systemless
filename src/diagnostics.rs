//! Opt-in run diagnostics: a census of every trap a guest reaches, and a
//! warning the first time a selector-dispatched trap takes a path that does
//! not actually implement what was asked for.
//!
//! Both are disabled unless their environment variable is set, and neither
//! changes what the emulator does — they only report.
//!
//! The motivation is that a high-level emulator's hardest bugs are the quiet
//! ones. A trap that is missing outright announces itself; a trap that accepts
//! a selector it does not implement, returns a plausible value and lets the
//! guest continue does not. Two such cases cost a long debugging session each:
//! `ScriptUtil` answering an unimplemented text-utility selector with zero,
//! and `PBStatus` ignoring `csCode` entirely so a gamma fade ramped a table it
//! had never been given. The first at least printed a line. The second was
//! silent, and had to be found by tracing 68K instructions by hand.
//!
//! * `SYSTEMLESS_TRAP_CENSUS=1` — count every dispatched trap and print the
//!   totals at the end of a headless run.
//! * `SYSTEMLESS_TRACE_STUBS=1` — report the first time each distinct
//!   (trap, selector) pair takes an unimplemented path.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static CENSUS_ENABLED: OnceLock<bool> = OnceLock::new();
static STUBS_ENABLED: OnceLock<bool> = OnceLock::new();

fn census_enabled() -> bool {
    *CENSUS_ENABLED.get_or_init(|| std::env::var_os("SYSTEMLESS_TRAP_CENSUS").is_some())
}

fn stubs_enabled() -> bool {
    *STUBS_ENABLED.get_or_init(|| std::env::var_os("SYSTEMLESS_TRACE_STUBS").is_some())
}

/// (is_tool, trap_num) -> call count.
fn census() -> &'static Mutex<HashMap<(bool, u16), u64>> {
    static CENSUS: OnceLock<Mutex<HashMap<(bool, u16), u64>>> = OnceLock::new();
    CENSUS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Distinct (label, trap word, selector) triples already reported.
fn reported_stubs() -> &'static Mutex<HashSet<(&'static str, u32, u32)>> {
    static REPORTED: OnceLock<Mutex<HashSet<(&'static str, u32, u32)>>> = OnceLock::new();
    REPORTED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Reconstruct the trap word a dispatched trap came from, so a census line can
/// be grepped against Inside Macintosh directly. Tool traps set bit 11.
fn trap_word(is_tool: bool, trap_num: u16) -> u16 {
    if is_tool {
        0xA000 | 0x0800 | (trap_num & 0x03FF)
    } else {
        0xA000 | (trap_num & 0x03FF)
    }
}

/// Count one dispatched trap. Called on every trap, so it returns immediately
/// unless the census was asked for.
#[inline]
pub fn record_trap(is_tool: bool, trap_num: u16) {
    if !census_enabled() {
        return;
    }
    if let Ok(mut counts) = census().lock() {
        *counts.entry((is_tool, trap_num)).or_insert(0) += 1;
    }
}

/// Report, once per distinct selector, that a trap took a path which does not
/// implement what the guest asked for.
///
/// `label` names the trap in the message, `word` is its trap word, and
/// `selector` is whatever sub-value went unhandled — a `ScriptUtil` selector,
/// a driver `csCode`, a Pack routine number.
pub fn record_stub(label: &'static str, word: u32, selector: u32) {
    if !stubs_enabled() {
        return;
    }
    let Ok(mut seen) = reported_stubs().lock() else {
        return;
    };
    if seen.insert((label, word, selector)) {
        eprintln!(
            "[STUB] {} (${:04X}) selector ${:08X} is not implemented; \
             the guest was given a default answer",
            label, word, selector
        );
    }
}

/// Print the census, most-called first. Safe to call when the census was never
/// enabled: it prints nothing.
pub fn print_trap_census() {
    if !census_enabled() {
        return;
    }
    static PRINTED: AtomicBool = AtomicBool::new(false);
    if PRINTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Ok(counts) = census().lock() else {
        return;
    };
    let mut rows: Vec<((bool, u16), u64)> = counts.iter().map(|(k, v)| (*k, *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let total: u64 = rows.iter().map(|(_, count)| count).sum();
    eprintln!(
        "[CENSUS] {} distinct traps, {} calls",
        rows.len(),
        total
    );
    for ((is_tool, trap_num), count) in rows {
        eprintln!(
            "[CENSUS]   ${:04X} {} {:>12}",
            trap_word(is_tool, trap_num),
            if is_tool { "tool" } else { "os  " },
            count
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trap_word_sets_the_tool_bit_for_tool_traps() {
        // ScriptUtil is $A8B5: a tool trap, low ten bits 0x0B5.
        assert_eq!(trap_word(true, 0x0B5), 0xA8B5);
        // GetDeviceList is $AA29.
        assert_eq!(trap_word(true, 0x229), 0xAA29);
    }

    #[test]
    fn trap_word_leaves_the_tool_bit_clear_for_os_traps() {
        // PBStatus is $A005, an OS trap.
        assert_eq!(trap_word(false, 0x005), 0xA005);
    }

    #[test]
    fn recording_is_inert_when_the_census_is_not_enabled() {
        // The default for a run with no environment variable set is that
        // nothing is collected and nothing is printed.
        if census_enabled() {
            return;
        }
        record_trap(true, 0x0B5);
        assert!(census().lock().expect("census lock").is_empty());
        print_trap_census();
    }
}
