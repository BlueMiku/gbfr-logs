//! Crash-proof memory reads shared across hooks. `IsBadReadPtr`-guarded rather than
//! `VirtualQuery`-guarded: `VirtualQuery` takes the process's address-space lock, which
//! under allocation churn makes a naive per-read guard measurably slower (a lesson
//! carried over from villith/relink-logs, which hit this exact slowdown). Safe to point
//! at a stale, unverified, or garbage pointer - never faults the game thread.

/// Deprecated-but-ubiquitous kernel32 export, not exposed by the `windows` crate.
pub(crate) fn readable(addr: usize, len: usize) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn IsBadReadPtr(lp: *const std::ffi::c_void, ucb: usize) -> i32;
    }
    if addr == 0 || addr.checked_add(len).is_none() {
        return false;
    }
    unsafe { IsBadReadPtr(addr as *const _, len) == 0 }
}

pub(crate) fn read_u32_guarded(base: usize, offset: usize) -> u32 {
    if base == 0 {
        return 0;
    }
    let addr = base.wrapping_add(offset);
    if !readable(addr, 4) {
        return 0;
    }
    unsafe { (addr as *const u32).read_unaligned() }
}

pub(crate) fn read_ptr_guarded(base: usize, offset: usize) -> Option<usize> {
    if base == 0 {
        return None;
    }
    let addr = base.wrapping_add(offset);
    if !readable(addr, std::mem::size_of::<usize>()) {
        return None;
    }
    Some(unsafe { (addr as *const usize).read_unaligned() })
}

pub(crate) fn read_f32_guarded(base: usize, offset: usize) -> Option<f32> {
    if base == 0 {
        return None;
    }
    let addr = base.wrapping_add(offset);
    if !readable(addr, 4) {
        return None;
    }
    Some(unsafe { (addr as *const f32).read_unaligned() })
}

/// True iff `instance`'s vtable has a non-null function pointer at `offset` - lets a
/// caller confirm a virtual call is safe to make without actually making it. Guards both
/// hops (the vtable pointer itself, then the slot within it), so a stale offset or a
/// source mid-teardown fails closed instead of crashing on the eventual call.
pub(crate) fn vtable_slot_readable(instance: *const usize, offset: usize) -> bool {
    let Some(vtable) = read_ptr_guarded(instance as usize, 0) else {
        return false;
    };
    if vtable == 0 {
        return false;
    }
    read_ptr_guarded(vtable, offset).map(|slot| slot != 0).unwrap_or(false)
}
