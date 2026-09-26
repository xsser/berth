use std::os::unix::fs::PermissionsExt;

use berth_core::{
    AgentKind, AgentState, CursorState, LineSnapshot, PersistPolicy, ScreenSnapshot, SessionStatus,
    Style, StyleId, StyleInterner, TermModes,
};

use super::*;

fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&Paths::in_dir(dir.path())).unwrap();
    (dir, store)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn workspace(name: &str, order: u32) -> Workspace {
    Workspace {
        id: WorkspaceId::new(),
        name: name.into(),
        root: PathBuf::from(format!("/tmp/{name}")),
        color: Some([0x12, 0xab, 0xff]),
        order,
        created_at_ms: 1_000 + i64::from(order),
    }
}

fn session(ws: WorkspaceId, order: u32) -> SessionMeta {
    let mut m = SessionMeta {
        id: SessionId::new(),
        workspace: ws,
        title_auto: "zsh".into(),
        cwd: PathBuf::from("/tmp/项目 dir"),
        command: vec!["/bin/zsh".into(), "-l".into()],
        env: vec![("FOO".into(), "bar".into())],
        status: SessionStatus::Dormant {
            exit_code: Some(3),
            at_ms: 42,
        },
        created_at_ms: 7,
        last_active_ms: 9,
        unread: true,
        persist: PersistPolicy {
            snapshot: true,
            journal: true,
        },
        order,
        cols: 120,
        rows: 40,
        ..Default::default()
    };
    m.agent.kind = AgentKind::Claude;
    m.agent.external_id = Some("abc-123".into());
    m.agent.state = AgentState::ToolRunning {
        tool: "Bash".into(),
    };
    m.agent.context_pct = Some(42.5);
    m.agent.cost_usd = Some(1.25);
    m
}

fn line(text: &str, style: StyleId) -> LineSnapshot {
    let mut l = LineSnapshot::blank();
    l.push_str(text, style);
    l
}

fn snapshot(meta: &SessionMeta, history_lines: usize) -> SessionSnapshotFile {
    let mut interner = StyleInterner::new();
    let red = interner.intern(Style {
        fg: berth_core::Color::Indexed(1),
        ..Default::default()
    });
    let history = (0..history_lines)
        .map(|i| {
            let mut l = line(&format!("line {i:05} "), StyleId::DEFAULT);
            l.push_str("红色 text", red);
            l.wrapped = i % 7 == 0;
            l
        })
        .collect();
    SessionSnapshotFile {
        format_version: SNAPSHOT_FORMAT_VERSION,
        saved_at_ms: 123,
        session: meta.clone(),
        styles: interner.table().clone(),
        history,
        screen: Some(ScreenSnapshot {
            cols: 120,
            rows: 2,
            lines: vec![line("$ echo hi", StyleId::DEFAULT), LineSnapshot::blank()],
            cursor: CursorState {
                row: 1,
                col: 0,
                visible: true,
                ..Default::default()
            },
            modes: TermModes::SHOW_CURSOR | TermModes::BRACKETED_PASTE,
            display_offset: 0,
            history_len: history_lines as u64,
            title: "zsh".into(),
        }),
    }
}

#[test]
fn open_sets_private_permissions_and_wal() {
    let (dir, store) = setup();
    let p = store.paths().clone();
    assert_eq!(mode(dir.path()), 0o700);
    assert_eq!(mode(&p.snapshots_dir), 0o700);
    assert_eq!(mode(&p.journals_dir), 0o700);
    assert_eq!(mode(&p.db), 0o600);
    let journal_mode: String = store
        .db()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_lowercase(), "wal");
    // A write creates the -wal file; it must inherit 0600.
    store.upsert_workspace(&workspace("a", 0)).unwrap();
    let wal = PathBuf::from(format!("{}-wal", p.db.display()));
    assert!(wal.exists());
    assert_eq!(mode(&wal), 0o600);
}

#[test]
fn workspace_crud_roundtrip() {
    let (_dir, store) = setup();
    let a = workspace("a", 1);
    let mut b = workspace("b", 0);
    store.upsert_workspace(&a).unwrap();
    store.upsert_workspace(&b).unwrap();
    assert_eq!(store.list_workspaces().unwrap(), vec![b.clone(), a.clone()]);

    b.name = "renamed".into();
    b.color = None;
    b.order = 5;
    store.upsert_workspace(&b).unwrap();
    assert_eq!(store.list_workspaces().unwrap(), vec![a.clone(), b.clone()]);

    store.delete_workspace(a.id).unwrap();
    assert_eq!(store.list_workspaces().unwrap(), vec![b]);
}

#[test]
fn session_crud_roundtrip_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let ws = WorkspaceId::new();
    let s1 = session(ws, 2);
    let mut s2 = session(ws, 1);
    {
        let store = Store::open(&paths).unwrap();
        store.upsert_session(&s1).unwrap();
        store.upsert_session(&s2).unwrap();
        assert_eq!(store.list_sessions().unwrap(), vec![s2.clone(), s1.clone()]);
        s2.status = SessionStatus::Live;
        s2.title_user = Some("renamed".into());
        store.upsert_session(&s2).unwrap();
        assert_eq!(store.get_session(s2.id).unwrap(), Some(s2.clone()));
        assert_eq!(store.get_session(SessionId::new()).unwrap(), None);
    }
    // Survives a reopen (migrations are idempotent).
    let store = Store::open(&paths).unwrap();
    assert_eq!(store.list_sessions().unwrap(), vec![s2, s1]);
}

#[test]
fn snapshot_roundtrip_20k_lines_atomic_and_private() {
    let (_dir, store) = setup();
    let meta = session(WorkspaceId::new(), 0);
    let snap = snapshot(&meta, 20_000);
    store.write_snapshot(&snap).unwrap();

    let path = store.paths().snapshot_file(&meta.id);
    assert_eq!(mode(&path), 0o600);
    assert!(
        !fsutil::tmp_path(&path).exists(),
        "tmp file must be renamed away"
    );
    let compressed = std::fs::metadata(&path).unwrap().len();
    let raw = postcard::to_stdvec(&snap).unwrap().len() as u64;
    assert!(
        compressed * 4 < raw,
        "zstd should compress repetitive lines: {compressed} vs {raw}"
    );

    let back = store.read_snapshot(meta.id).unwrap().unwrap();
    assert_eq!(back.history.len(), 20_000);
    assert_eq!(back, snap);
    assert_eq!(store.read_snapshot(SessionId::new()).unwrap(), None);

    // Overwrite replaces atomically.
    let smaller = snapshot(&meta, 3);
    store.write_snapshot(&smaller).unwrap();
    assert_eq!(store.read_snapshot(meta.id).unwrap().unwrap(), smaller);
}

#[test]
fn snapshot_format_version_is_checked() {
    let (_dir, store) = setup();
    let meta = session(WorkspaceId::new(), 0);
    let mut snap = snapshot(&meta, 1);
    snap.format_version = SNAPSHOT_FORMAT_VERSION + 1;
    store.write_snapshot(&snap).unwrap();
    match store.read_snapshot(meta.id) {
        Err(StoreError::Format { found, want }) => {
            assert_eq!(found, SNAPSHOT_FORMAT_VERSION + 1);
            assert_eq!(want, SNAPSHOT_FORMAT_VERSION);
        }
        other => panic!("expected format error, got {other:?}"),
    }
    // Garbage is a codec/io error, not a panic.
    std::fs::write(store.paths().snapshot_file(&meta.id), b"not zstd").unwrap();
    assert!(store.read_snapshot(meta.id).is_err());
}

#[test]
fn events_listed_newest_first_with_limit() {
    let (_dir, store) = setup();
    let sid = SessionId::new();
    let other = SessionId::new();
    for (at, kind) in [
        (10, "hook:SessionStart"),
        (30, "hook:Stop"),
        (20, "hook:PreToolUse"),
    ] {
        store
            .record_event(&EventRecord {
                session: sid,
                at_ms: at,
                kind: kind.into(),
                state: "idle".into(),
                detail: Some("Bash".into()),
            })
            .unwrap();
    }
    store
        .record_event(&EventRecord {
            session: other,
            at_ms: 99,
            kind: "pty:exit".into(),
            state: "exited".into(),
            detail: None,
        })
        .unwrap();
    let events = store.list_events(sid, 10).unwrap();
    let times: Vec<i64> = events.iter().map(|e| e.at_ms).collect();
    assert_eq!(times, vec![30, 20, 10]);
    assert!(events.iter().all(|e| e.session == sid));
    assert_eq!(store.list_events(sid, 2).unwrap().len(), 2);

    // Details are capped to identifiers.
    store
        .record_event(&EventRecord {
            session: sid,
            at_ms: 40,
            kind: "hook:Notification".into(),
            state: "waiting_input".into(),
            detail: Some("x".repeat(1000)),
        })
        .unwrap();
    let newest = &store.list_events(sid, 1).unwrap()[0];
    assert_eq!(newest.detail.as_ref().unwrap().len(), MAX_EVENT_DETAIL);
}

#[test]
fn purge_removes_everything() {
    let (_dir, store) = setup();
    let meta = session(WorkspaceId::new(), 0);
    let keep = session(meta.workspace, 1);
    store.upsert_session(&meta).unwrap();
    store.upsert_session(&keep).unwrap();
    store
        .record_event(&EventRecord {
            session: meta.id,
            at_ms: 1,
            kind: "pty:exit".into(),
            state: "exited".into(),
            detail: None,
        })
        .unwrap();
    store.write_snapshot(&snapshot(&meta, 10)).unwrap();
    std::fs::write(
        fsutil::tmp_path(&store.paths().snapshot_file(&meta.id)),
        b"partial",
    )
    .unwrap();
    let mut j = store.open_journal(meta.id).unwrap();
    j.append(1, b"hello").unwrap();
    j.flush().unwrap();
    drop(j);

    store.purge_session(meta.id).unwrap();
    assert_eq!(store.get_session(meta.id).unwrap(), None);
    assert!(store.list_events(meta.id, 10).unwrap().is_empty());
    assert!(!store.paths().snapshot_file(&meta.id).exists());
    assert!(!fsutil::tmp_path(&store.paths().snapshot_file(&meta.id)).exists());
    assert!(!store.paths().journal_dir(&meta.id).exists());
    // Other sessions untouched; purging twice is fine.
    assert_eq!(store.list_sessions().unwrap(), vec![keep]);
    store.purge_session(meta.id).unwrap();
}

#[test]
fn journal_appends_rotates_and_reopens() {
    let (_dir, store) = setup();
    let sid = SessionId::new();
    let dir = store.paths().journal_dir(&sid);
    let mut j = JournalWriter::with_max_bytes(&dir, 64).unwrap();
    let chunks: Vec<Vec<u8>> = (0..10u8).map(|i| vec![b'a' + i; 20]).collect();
    for (i, c) in chunks.iter().enumerate() {
        j.append(1_000 + i as i64, c).unwrap();
    }
    j.flush().unwrap();
    assert!(
        j.current_file().ends_with("0004.log"),
        "{:?}",
        j.current_file()
    );
    drop(j);

    assert_eq!(mode(&dir), 0o700);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["0000.log", "0001.log", "0002.log", "0003.log", "0004.log"]
    );
    for n in &names {
        assert_eq!(mode(&dir.join(n)), 0o600);
        assert!(std::fs::metadata(dir.join(n)).unwrap().len() <= 64);
    }

    // Reopen continues in the last file; a big record gets its own file.
    let mut j = JournalWriter::with_max_bytes(&dir, 64).unwrap();
    j.append(2_000, &[b'z'; 100]).unwrap();
    j.flush().unwrap();
    assert!(j.current_file().ends_with("0005.log"));
    drop(j);

    let records = read_journal(&dir).unwrap();
    assert_eq!(records.len(), 11);
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(
            records[i],
            JournalRecord {
                at_ms: 1_000 + i as i64,
                bytes: c.clone()
            }
        );
    }
    assert_eq!(records[10].bytes.len(), 100);
    assert_eq!(JOURNAL_ROTATE_BYTES, 64 * 1024 * 1024);
}

#[test]
fn newer_schema_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    drop(Store::open(&paths).unwrap());
    {
        let conn = Connection::open(&paths.db).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            params![SCHEMA_VERSION + 1],
        )
        .unwrap();
    }
    assert!(matches!(
        Store::open(&paths),
        Err(StoreError::Schema { .. })
    ));
}
