use super::*;

fn agent(name: &str) -> AgentId {
    AgentId::from(name.to_owned())
}

fn tmp_dir() -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ja-dirlock-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn acquire_then_busy_then_release() {
    let mgr = DirLockManager::new();
    let a = agent("a");
    let b = agent("b");
    let dir = tmp_dir();
    let canon = std::fs::canonicalize(&dir).unwrap();

    assert_eq!(
        mgr.acquire(&a, &dir, &[]).unwrap(),
        AcquireOutcome::Acquired
    );
    // Re-acquire by the same agent is idempotent.
    assert_eq!(
        mgr.acquire(&a, &dir, &[]).unwrap(),
        AcquireOutcome::AlreadyHeld
    );
    // Another agent is told who holds it (and on which canonical path).
    assert_eq!(
        mgr.acquire(&b, &dir, &[]).unwrap(),
        AcquireOutcome::Busy {
            holder: a.clone(),
            conflict: canon.clone(),
        }
    );
    // write_paths reflects only the holder.
    assert_eq!(mgr.write_paths(&a).unwrap(), vec![canon.clone()]);
    assert!(mgr.write_paths(&b).unwrap().is_empty());
    // holder surfaces the id.
    assert_eq!(mgr.holder(&dir).unwrap(), Some(a.clone()));

    mgr.release(&a, &dir).unwrap();
    // Now b can acquire.
    assert_eq!(
        mgr.acquire(&b, &dir, &[]).unwrap(),
        AcquireOutcome::Acquired
    );
    assert_eq!(mgr.holder(&dir).unwrap(), Some(b.clone()));
}

#[test]
fn release_all_drops_every_lock_for_agent() {
    let mgr = DirLockManager::new();
    let a = agent("a");
    let d1 = tmp_dir();
    let d2 = tmp_dir();
    mgr.acquire(&a, &d1, &[]).unwrap();
    mgr.acquire(&a, &d2, &[]).unwrap();
    assert_eq!(mgr.write_paths(&a).unwrap().len(), 2);

    mgr.release_all(&a);
    assert!(mgr.write_paths(&a).unwrap().is_empty());
    assert_eq!(mgr.holder(&d1).unwrap(), None);
}

#[test]
fn release_by_non_owner_is_noop() {
    let mgr = DirLockManager::new();
    let a = agent("a");
    let b = agent("b");
    let dir = tmp_dir();
    mgr.acquire(&a, &dir, &[]).unwrap();
    // b releasing a's lock must not steal it.
    mgr.release(&b, &dir).unwrap();
    assert_eq!(mgr.holder(&dir).unwrap(), Some(a));
}

#[test]
fn release_on_deleted_path_is_idempotent() {
    let mgr = DirLockManager::new();
    let a = agent("a");
    let dir = tmp_dir();
    mgr.acquire(&a, &dir, &[]).unwrap();
    // The directory vanishes out from under the holder.
    std::fs::remove_dir_all(&dir).unwrap();
    // release must not error (idempotent), and the lock is dropped.
    mgr.release(&a, &dir).unwrap();
}

#[test]
fn non_existent_path_is_rejected() {
    let mgr = DirLockManager::new();
    let a = agent("a");
    let ghost = std::env::temp_dir().join("ja-dirlock-ghost-does-not-exist");
    assert!(mgr.acquire(&a, &ghost, &[]).is_err());
}

#[test]
fn per_agent_cap_is_enforced() {
    let mgr = DirLockManager::new();
    let a = agent("a");
    for _ in 0..MAX_LOCKS_PER_AGENT {
        mgr.acquire(&a, &tmp_dir(), &[]).unwrap();
    }
    let err = mgr.acquire(&a, &tmp_dir(), &[]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ResourceBusy);
}

#[test]
fn holder_canonicalizes_query() {
    let mgr = DirLockManager::new();
    let a = agent("a");
    let dir = tmp_dir();
    let canon = std::fs::canonicalize(&dir).unwrap();
    mgr.acquire(&a, &dir, &[]).unwrap();
    // Querying with the canonical path (not the original) still resolves.
    assert_eq!(mgr.holder(&canon).unwrap(), Some(a));
}

// -- hierarchical exclusion (ancestor / descendant / sibling / boundary) --

/// A helper tree: parent/a/b, parent/a/bb, parent/a/b0, parent/a/d, parent/x
/// (with parent/x/y), plus a grandchild parent/a/b/c/d — all created under one
/// temp root.
struct Tree {
    a_b: PathBuf,
    a_bb: PathBuf,
    a_b0: PathBuf,
    a_d: PathBuf,
    x: PathBuf,
    x_y: PathBuf,
    a_b_c_d: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let root = tmp_dir();
        let a_b = root.join("a").join("b");
        let a_bb = root.join("a").join("bb");
        let a_b0 = root.join("a").join("b0");
        let a_d = root.join("a").join("d");
        let x = root.join("x");
        let x_y = x.join("y");
        let a_b_c_d = a_b.join("c").join("d");
        for d in [&a_b, &a_bb, &a_b0, &a_d, &x, &x_y, &a_b_c_d] {
            std::fs::create_dir_all(d).unwrap();
        }
        Self {
            a_b,
            a_bb,
            a_b0,
            a_d,
            x,
            x_y,
            a_b_c_d,
        }
    }
}

#[test]
fn ancestor_lock_blocks_descendant() {
    // Also the "peer with empty chain stays Busy" case.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let a1 = agent("1");
    let a2 = agent("2");
    let a_b_canon = std::fs::canonicalize(&t.a_b).unwrap();

    mgr.acquire(&a1, &t.a_b, &[]).unwrap();
    // Descendant of a/b is blocked; conflict names the holder's lock (a/b).
    assert_eq!(
        mgr.acquire(&a2, &t.a_b.join("c"), &[]).unwrap(),
        AcquireOutcome::Busy {
            holder: a1.clone(),
            conflict: a_b_canon,
        }
    );
}

#[test]
fn descendant_lock_blocks_ancestor() {
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let a1 = agent("1");
    let a2 = agent("2");
    let child = t.a_b.join("c");
    let child_canon = std::fs::canonicalize(&child).unwrap();

    mgr.acquire(&a1, &child, &[]).unwrap();
    // Ancestor a/b is blocked by the held descendant; conflict names the child.
    assert_eq!(
        mgr.acquire(&a2, &t.a_b, &[]).unwrap(),
        AcquireOutcome::Busy {
            holder: a1,
            conflict: child_canon,
        }
    );
}

#[test]
fn grandparent_blocks_grandchild() {
    // Exercises the full ancestors() walk, not just the immediate parent.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let a1 = agent("1");
    let a2 = agent("2");
    let a_b_canon = std::fs::canonicalize(&t.a_b).unwrap();

    mgr.acquire(&a1, &t.a_b, &[]).unwrap();
    assert_eq!(
        mgr.acquire(&a2, &t.a_b_c_d, &[]).unwrap(),
        AcquireOutcome::Busy {
            holder: a1,
            conflict: a_b_canon,
        }
    );
}

#[test]
fn sibling_locks_independent() {
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let a1 = agent("1");
    let a2 = agent("2");

    mgr.acquire(&a1, &t.a_b, &[]).unwrap();
    // Sibling under the same parent: no overlap.
    assert_eq!(
        mgr.acquire(&a2, &t.a_d, &[]).unwrap(),
        AcquireOutcome::Acquired
    );
}

#[test]
fn component_boundary_no_false_conflict() {
    // The critical case: /a/b must NOT match /a/bb or /a/b0 (component-wise,
    // not byte-prefix). Acquiring a sibling with a string-prefix-y name succeeds.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let a1 = agent("1");
    let a2 = agent("2");

    mgr.acquire(&a1, &t.a_b, &[]).unwrap();
    assert_eq!(
        mgr.acquire(&a2, &t.a_bb, &[]).unwrap(),
        AcquireOutcome::Acquired
    );
    assert_eq!(
        mgr.acquire(&a2, &t.a_b0, &[]).unwrap(),
        AcquireOutcome::Acquired
    );
}

#[test]
fn self_overlap_requires_release() {
    // Acquiring a nested or wider path that overlaps your own lock is an
    // error (release first); a non-overlapping path is fine; the exact path
    // is AlreadyHeld.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let a1 = agent("1");

    mgr.acquire(&a1, &t.a_b, &[]).unwrap();
    // Descendant of own lock → error.
    assert!(mgr.acquire(&a1, &t.a_b.join("c"), &[]).is_err());
    // Wider (ancestor) of own lock → error.
    assert!(mgr.acquire(&a1, t.a_b.parent().unwrap(), &[]).is_err());
    // Non-overlapping sibling name → acquired.
    assert_eq!(
        mgr.acquire(&a1, &t.a_bb, &[]).unwrap(),
        AcquireOutcome::Acquired
    );
    // Exact same path → idempotent.
    assert_eq!(
        mgr.acquire(&a1, &t.a_b, &[]).unwrap(),
        AcquireOutcome::AlreadyHeld
    );
}

#[test]
fn multi_lock_partial_overlap() {
    // Two unrelated held locks; overlaps conflict, non-overlaps succeed, and
    // the boundary /a/b0 does not false-match /a/b.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let a1 = agent("1");
    let a2 = agent("2");

    mgr.acquire(&a1, &t.a_b, &[]).unwrap();
    mgr.acquire(&a1, &t.x, &[]).unwrap();

    // Overlap with a1's a/b lock → Busy.
    assert!(matches!(
        mgr.acquire(&a2, &t.a_b.join("c"), &[]).unwrap(),
        AcquireOutcome::Busy { .. }
    ));
    // Sibling under a different tree (x) — a1 holds x, so this overlaps too.
    assert!(matches!(
        mgr.acquire(&a2, &t.x_y, &[]).unwrap(),
        AcquireOutcome::Busy { .. }
    ));
    // Boundary: a/b0 does NOT overlap a/b → acquired.
    assert_eq!(
        mgr.acquire(&a2, &t.a_b0, &[]).unwrap(),
        AcquireOutcome::Acquired
    );
}

#[test]
fn root_lock_blocks_all() {
    // Holding / excludes everything (descendant scan degenerates to a full
    // scan; confirm it still terminates with Busy).
    let mgr = DirLockManager::new();
    let a1 = agent("1");
    let a2 = agent("2");
    let root_canon = std::fs::canonicalize("/").unwrap();

    mgr.acquire(&a1, Path::new("/"), &[]).unwrap();
    assert_eq!(
        mgr.acquire(&a2, &tmp_dir(), &[]).unwrap(),
        AcquireOutcome::Busy {
            holder: a1,
            conflict: root_canon,
        }
    );
}

// -- delegation (chain relaxes the ancestor direction only) --

#[test]
fn delegation_allows_nested_under_ancestor() {
    // The core fix: a child may lock a sub-directory of its supervisor's lock.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let parent = agent("parent");
    let child = agent("child");

    mgr.acquire(&parent, &t.a_b, &[]).unwrap();
    // child's chain contains parent → the nested acquire is delegation.
    assert_eq!(
        mgr.acquire(&child, &t.a_b.join("c"), std::slice::from_ref(&parent))
            .unwrap(),
        AcquireOutcome::Acquired
    );
    // The carve-out surfaces in the parent's readonly view (peers' locked dirs).
    let c_canon = std::fs::canonicalize(t.a_b.join("c")).unwrap();
    assert_eq!(mgr.readonly_paths(&parent).unwrap(), vec![c_canon.clone()]);
    // And the parent still owns its broader lock; the child owns the carve-out.
    assert_eq!(mgr.write_paths(&child).unwrap(), vec![c_canon]);
}

#[test]
fn delegation_chain_continues_past_delegated_ancestor() {
    // A delegated ancestor is skipped (continue), but a HIGHER non-chain
    // ancestor must still block. Build: grandparent holds a/b; parent (chain
    // member) holds nothing; child acquires a/b/c/d with chain={parent}.
    // grandparent is NOT in the chain → must still block.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let grandparent = agent("gp");
    let parent = agent("parent");
    let child = agent("child");
    let a_b_canon = std::fs::canonicalize(&t.a_b).unwrap();

    mgr.acquire(&grandparent, &t.a_b, &[]).unwrap();
    // parent is in the chain but holds no lock; grandparent holds a/b and is
    // not in the chain → Busy naming grandparent.
    assert_eq!(
        mgr.acquire(&child, &t.a_b_c_d, std::slice::from_ref(&parent))
            .unwrap(),
        AcquireOutcome::Busy {
            holder: grandparent,
            conflict: a_b_canon,
        }
    );
}

#[test]
fn delegation_multi_level_allows_grandchild() {
    // Grandchild whose chain spans parent and grandparent may lock under a
    // grandparent-held lock through an un-locked middle parent.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let grandparent = agent("gp");
    let parent = agent("parent");
    let grandchild = agent("gc");

    mgr.acquire(&grandparent, &t.a_b, &[]).unwrap();
    assert_eq!(
        mgr.acquire(
            &grandchild,
            &t.a_b_c_d,
            &[parent.clone(), grandparent.clone()]
        )
        .unwrap(),
        AcquireOutcome::Acquired
    );
}

#[test]
fn delegation_does_not_allow_parent_over_child() {
    // Only the ancestor direction is relaxed: a parent (whose chain does not
    // contain the child) cannot widen over a region its child has locked.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let parent = agent("parent");
    let child = agent("child");
    let c_canon = std::fs::canonicalize(t.a_b.join("c")).unwrap();

    mgr.acquire(&child, &t.a_b.join("c"), &[]).unwrap();
    // parent tries to acquire a/b (wider, over child's c) — blocked.
    assert_eq!(
        mgr.acquire(&parent, &t.a_b, std::slice::from_ref(&child))
            .unwrap(),
        AcquireOutcome::Busy {
            holder: child,
            conflict: c_canon,
        }
    );
}

#[test]
fn delegation_exact_path_not_exempt() {
    // A child cannot steal its supervisor's exact path via the chain: the
    // exact-path branch returns Busy before overlap_conflict (and thus before
    // the chain) is consulted.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let parent = agent("parent");
    let child = agent("child");
    let a_b_canon = std::fs::canonicalize(&t.a_b).unwrap();

    mgr.acquire(&parent, &t.a_b, &[]).unwrap();
    assert_eq!(
        mgr.acquire(&child, &t.a_b, std::slice::from_ref(&parent))
            .unwrap(),
        AcquireOutcome::Busy {
            holder: parent,
            conflict: a_b_canon,
        }
    );
}

#[test]
fn delegation_sibling_carveouts_both_visible() {
    // Two siblings under one parent: both acquire, and the parent's readonly
    // view contains both carve-outs.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let parent = agent("parent");
    let c1 = agent("c1");
    let c2 = agent("c2");
    let a_canon = std::fs::canonicalize(&t.a_b).unwrap();
    let d_canon = std::fs::canonicalize(&t.a_d).unwrap();

    mgr.acquire(&parent, t.a_b.parent().unwrap(), &[]).unwrap();
    assert_eq!(
        mgr.acquire(&c1, &t.a_b, std::slice::from_ref(&parent))
            .unwrap(),
        AcquireOutcome::Acquired
    );
    assert_eq!(
        mgr.acquire(&c2, &t.a_d, std::slice::from_ref(&parent))
            .unwrap(),
        AcquireOutcome::Acquired
    );
    let mut ro = mgr.readonly_paths(&parent).unwrap();
    ro.sort();
    assert_eq!(ro, vec![a_canon, d_canon]);
}

#[test]
fn delegation_does_not_inflate_ancestor_lock_count() {
    // Delegating to a child does not count against the ancestor's lock cap —
    // the ancestor still holds exactly one lock.
    let mgr = DirLockManager::new();
    let t = Tree::new();
    let parent = agent("parent");
    let child = agent("child");

    mgr.acquire(&parent, &t.a_b, &[]).unwrap();
    mgr.acquire(&child, &t.a_b.join("c"), std::slice::from_ref(&parent))
        .unwrap();
    assert_eq!(mgr.write_paths(&parent).unwrap().len(), 1);
}
