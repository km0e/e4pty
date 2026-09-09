//! fd-table census for the Unix backend. Linux only (`/proc`); gated at
//! the crate level so other platforms see an empty test binary.
#![cfg(target_os = "linux")]

use e4pty::prelude::*;

fn fd_links() -> Vec<(u32, String)> {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let fd: u32 = e.file_name().to_string_lossy().parse().ok()?;
            let link = std::fs::read_link(e.path()).ok()?;
            Some((fd, link.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Master vs slave classification works on both a host (where the master
/// link is `/dev/ptmx`) and inside containers (where `/dev/ptmx` is a
/// symlink into a private devpts instance, so the master link renders as
/// e.g. `/dev/pts/ptmx`).
fn is_master(link: &str) -> bool {
    link.ends_with("/ptmx")
}

fn is_slave(link: &str) -> bool {
    link.starts_with("/dev/pts/") && !is_master(link)
}

/// Two invariants in one place (serialized by construction):
/// 1. after `openpty` the parent holds ZERO `/dev/pts/*` (slave) fds —
///    the three `Stdio` dups and the `pre_exec`-captured original all die
///    with the `Command` temporary at the end of the spawn statement.
///    This is the EOF-liveness invariant documented in `unix.rs`.
/// 2. dropping the pty releases every master fd the library created.
#[tokio::test]
async fn parent_slave_fds_and_master_fd_release() {
    let before: Vec<_> = fd_links()
        .into_iter()
        .filter(|(_, l)| is_master(l))
        .collect();
    let baseline = before.len(); // unrelated ptmx fds may pre-exist

    let pty = openpty(WindowSize::default(), Script::exec("sh", ["-c", "sleep 5"]))
        .expect("openpty failed");

    let links = fd_links();
    let slaves: Vec<u32> = links
        .iter()
        .filter(|(_, l)| is_slave(l))
        .map(|(fd, _)| *fd)
        .collect();
    let masters = links.iter().filter(|(_, l)| is_master(l)).count();
    assert!(
        slaves.is_empty(),
        "parent holds slave fd copies (fds {slaves:?})"
    );
    assert!(
        masters >= baseline + 2,
        "expected >=2 new master fds (reader+writer), got {masters} (baseline {baseline})"
    );

    drop(pty);
    tokio::task::yield_now().await;
    let open_after = fd_links().into_iter().filter(|(_, l)| is_master(l)).count();
    assert_eq!(open_after, baseline, "master fds leaked after drop");
}
