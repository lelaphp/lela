use std::collections::{BTreeMap, BTreeSet};

use super::{Kind, Object, Standing, Swept, left};

/// `ipcs -a` as macOS printed it on 2026-10-01, cut to one object or two
/// of each kind: a database's segment and semaphores, and what the
/// subsets' tests left.
const IPCS: &str = "\
IPC status from <running system> as of Thu Oct  1 23:49:12 CEST 2026
T     ID     KEY        MODE       OWNER    GROUP  CREATOR   CGROUP CBYTES  QNUM QBYTES LSPID LRPID   STIME    RTIME    CTIME
Message Queues:
q 7536640 0x00000000 ----------- mitchell    staff mitchell    staff      0      0   2048      0      0 no-entry no-entry 23:42:09
q 1441795 0x00000001 --rw-rw-rw- mitchell    staff mitchell    staff      0      0   2048   4242   4243 23:48:10 23:48:10 23:48:10

T     ID     KEY        MODE       OWNER    GROUP  CREATOR   CGROUP NATTCH  SEGSZ  CPID  LPID   ATIME    DTIME    CTIME
Shared Memory:
m  65536 0x042e2346 --rw------- mitchell    staff mitchell    staff      6     56    824    824 20:45:10 21:56:32 20:45:10
m 6750211 0x00000000 --rw-rw-rw- mitchell    staff mitchell    staff      0   1024  77085  77085 23:48:08 23:48:08 23:48:08

T     ID     KEY        MODE       OWNER    GROUP  CREATOR   CGROUP NSEMS   OTIME    CTIME
Semaphores:
s  65539 0x042e2347 --ra------- mitchell    staff mitchell    staff     20 20:45:10 20:45:10
s 131082 0x00000000 ----------- mitchell    staff mitchell    staff      3 no-entry  1:46:23
";

fn object(kind: Kind, id: u64, named: &[u32], attached: u64) -> Object {
    Object {
        kind,
        id,
        named: named.iter().copied().collect(),
        attached,
    }
}

#[test]
fn ipcs_is_read_by_each_sections_header() {
    let standing = Standing::parse(IPCS);
    assert_eq!(
        standing.0,
        [
            object(Kind::Queue, 7_536_640, &[], 0),
            object(Kind::Queue, 1_441_795, &[4242, 4243], 0),
            object(Kind::Memory, 65_536, &[824], 6),
            object(Kind::Memory, 6_750_211, &[77_085], 0),
            object(Kind::Semaphores, 65_539, &[], 0),
            object(Kind::Semaphores, 131_082, &[], 0),
        ]
    );
}

/// The rule, one counterexample per clause: only an object new since the
/// subset began, with nothing attached, naming a process of the run and
/// no living process, is the run's.
#[test]
fn only_what_the_runs_own_processes_left_is_removed() {
    let before = Standing(vec![
        // A database's segment, standing before the run, whose creator's
        // id the run's process was later handed again.
        object(Kind::Memory, 1, &[100], 0),
    ]);
    let after = Standing(vec![
        object(Kind::Memory, 1, &[100], 0),
        // A segment a test created and left.
        object(Kind::Memory, 2, &[100], 0),
        // One another process created during the run.
        object(Kind::Memory, 3, &[900], 0),
        // One a test created and something still holds.
        object(Kind::Memory, 4, &[101], 1),
        // A queue a test sent to and another program, still alive, read.
        object(Kind::Queue, 5, &[101, 500], 0),
        // A queue a test sent to and nobody read.
        object(Kind::Queue, 6, &[101], 0),
        // A semaphore set a test operated on.
        object(Kind::Semaphores, 7, &[102], 0),
        // A queue nobody sent to: nothing says whose it is.
        object(Kind::Queue, 8, &[], 0),
    ]);
    let spawned: BTreeSet<u32> = [100, 101, 102].into();
    let alive = |pid| pid == 500;
    let gone: Vec<(Kind, u64)> = left(&before, &after, &spawned, alive)
        .into_iter()
        .map(|o| (o.kind, o.id))
        .collect();
    assert_eq!(
        gone,
        [(Kind::Memory, 2), (Kind::Queue, 6), (Kind::Semaphores, 7)]
    );
}

#[test]
fn the_sweep_says_how_many_it_removed_of_each_kind_and_what_it_kept() {
    let swept = Swept {
        removed: BTreeMap::from([(Kind::Memory, 2), (Kind::Queue, 1)]),
        unnamed: 1,
    };
    assert_eq!(
        swept.to_string(),
        "removed 3 System V objects its tests left (2 shared memory segments, 1 message queue); 1 new one names no process and stands, since nothing tells the run's from another program's"
    );
    assert_eq!(
        Swept::default().to_string(),
        "removed 0 System V objects its tests left"
    );
}
