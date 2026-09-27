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

/// M4 (DESIGN §17.1): the archive mark lives in `meta_json`, no schema
/// change. It survives upserts and a reopen, clearing it is stored too,
/// `list_sessions` keeps returning archived sessions (clients filter), and
/// a row written before the field existed reads as not archived.
#[test]
fn archived_at_ms_roundtrips_through_the_registry() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let ws = WorkspaceId::new();
    let mut archived = session(ws, 0);
    archived.status = SessionStatus::Restored;
    archived.archived_at_ms = Some(1_790_000_000_123);
    let mut restored = session(ws, 1);
    restored.archived_at_ms = Some(5);
    {
        let store = Store::open(&paths).unwrap();
        store.upsert_session(&archived).unwrap();
        store.upsert_session(&restored).unwrap();
        restored.archived_at_ms = None;
        store.upsert_session(&restored).unwrap();
        assert_eq!(
            store.get_session(archived.id).unwrap(),
            Some(archived.clone())
        );
        assert_eq!(
            store.get_session(restored.id).unwrap(),
            Some(restored.clone())
        );
    }
    let store = Store::open(&paths).unwrap();
    assert_eq!(
        store.list_sessions().unwrap(),
        vec![archived.clone(), restored.clone()]
    );

    // As an M3 berthd wrote it: the JSON has no `archived_at_ms`.
    let old = session(ws, 2);
    let mut json = serde_json::to_value(&old).unwrap();
    assert!(json
        .as_object_mut()
        .unwrap()
        .remove("archived_at_ms")
        .is_some());
    store
        .db()
        .execute(
            r#"INSERT INTO sessions (id, workspace, status, "order", last_active_ms, meta_json)
               VALUES (?1, ?2, 'dormant', 2, 9, ?3)"#,
            params![old.id.to_string(), ws.to_string(), json.to_string()],
        )
        .unwrap();
    let read = store.get_session(old.id).unwrap().unwrap();
    assert!(!read.is_archived());
    assert_eq!(read, old);
    assert_eq!(
        store.list_sessions().unwrap(),
        vec![archived, restored, old]
    );
}

/// M4: a snapshot keeps the archive mark (format 2 holds the session as
/// JSON) without a new format version, and a format 2 file an M3 berthd
/// wrote, without the field, reads as not archived.
#[test]
fn archived_at_ms_roundtrips_through_a_snapshot() {
    let (_dir, store) = setup();
    let mut meta = session(WorkspaceId::new(), 0);
    meta.status = SessionStatus::Restored;
    meta.archived_at_ms = Some(1_790_000_000_456);
    let snap = snapshot(&meta, 5);
    store.write_snapshot(&snap).unwrap();
    assert_eq!(format_on_disk(&store, meta.id), SNAPSHOT_FORMAT_VERSION);
    let back = store.read_snapshot(meta.id).unwrap().unwrap();
    assert_eq!(back.session.archived_at_ms, Some(1_790_000_000_456));
    assert_eq!(back, snap);

    let mut json = serde_json::to_value(&meta).unwrap();
    assert!(json
        .as_object_mut()
        .unwrap()
        .remove("archived_at_ms")
        .is_some());
    let m3 = FileV2Out {
        format_version: 2,
        saved_at_ms: snap.saved_at_ms,
        session_json: json.to_string(),
        styles: &snap.styles,
        history: &snap.history,
        screen: &snap.screen,
    };
    put_snapshot_file(&store, meta.id, &postcard::to_stdvec(&m3).unwrap());
    let back = store.read_snapshot(meta.id).unwrap().unwrap();
    assert_eq!(back.session.archived_at_ms, None);
    assert_eq!(
        back.session,
        SessionMeta {
            archived_at_ms: None,
            ..meta
        }
    );
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

/// `raw` (uncompressed) as the snapshot file of `id`, the way the store
/// writes it.
fn put_snapshot_file(store: &Store, id: SessionId, raw: &[u8]) {
    std::fs::create_dir_all(&store.paths().snapshots_dir).unwrap();
    let compressed = zstd::bulk::compress(raw, SNAPSHOT_ZSTD_LEVEL).unwrap();
    std::fs::write(store.paths().snapshot_file(&id), compressed).unwrap();
}

/// The format version the snapshot file of `id` is in.
fn format_on_disk(store: &Store, id: SessionId) -> u32 {
    let bytes = std::fs::read(store.paths().snapshot_file(&id)).unwrap();
    let raw = zstd::stream::decode_all(&bytes[..]).unwrap();
    postcard::take_from_bytes::<u32>(&raw).unwrap().0
}

/// `m` in the layout of snapshot format 1 (no `last_agent`).
fn v1_meta(m: &SessionMeta) -> v1::SessionMeta {
    let a = &m.agent;
    v1::SessionMeta {
        id: m.id,
        workspace: m.workspace,
        title_auto: m.title_auto.clone(),
        title_user: m.title_user.clone(),
        cwd: m.cwd.clone(),
        command: m.command.clone(),
        env: m.env.clone(),
        status: m.status.clone(),
        agent: v1::AgentInfo {
            kind: a.kind.clone(),
            external_id: a.external_id.clone(),
            transcript_path: a.transcript_path.clone(),
            model: a.model.clone(),
            context_pct: a.context_pct,
            cost_usd: a.cost_usd,
            state: a.state.clone(),
            since_ms: a.since_ms,
            source: a.source,
            confidence: a.confidence,
        },
        created_at_ms: m.created_at_ms,
        last_active_ms: m.last_active_ms,
        unread: m.unread,
        persist: m.persist,
        order: m.order,
        cols: m.cols,
        rows: m.rows,
    }
}

/// A file a berthd of M1 / M2 wrote (format 1: the session as postcard,
/// no `last_agent`) is read into the current types; saved again, it is
/// format 2.
#[test]
fn a_format_1_snapshot_is_read_into_the_current_types() {
    let (_dir, store) = setup();
    let mut meta = session(WorkspaceId::new(), 0);
    meta.agent.transcript_path = Some("/p/-w/abc-123.jsonl".into());
    meta.agent.model = Some("Opus".into());
    let current = snapshot(&meta, 50);
    let old = v1::SessionSnapshotFile {
        format_version: 1,
        saved_at_ms: current.saved_at_ms,
        session: v1_meta(&meta),
        styles: current.styles.clone(),
        history: current.history.clone(),
        screen: current.screen.clone(),
    };
    put_snapshot_file(&store, meta.id, &postcard::to_stdvec(&old).unwrap());

    let back = store.read_snapshot(meta.id).unwrap().unwrap();
    assert_eq!(back.format_version, 1);
    assert_eq!(back.saved_at_ms, old.saved_at_ms);
    assert_eq!(back.session.agent.last_agent, None);
    assert_eq!(back.session, meta);
    assert_eq!(back.history.len(), old.history.len());
    for (i, (got, want)) in back.history.iter().zip(&old.history).enumerate() {
        assert_eq!(got, want, "history line {i}");
    }
    assert_eq!(back.screen, old.screen);
    assert_eq!(back.styles, old.styles);

    store.write_snapshot(&back).unwrap();
    assert_eq!(format_on_disk(&store, meta.id), 2);
    let again = store.read_snapshot(meta.id).unwrap().unwrap();
    assert_eq!(again.format_version, 2);
    assert_eq!(
        SessionSnapshotFile {
            format_version: 1,
            ..again
        },
        back
    );
}

/// Format 2 round trip, whatever version the snapshot was read in; the
/// session goes in as JSON.
#[test]
fn snapshots_are_written_as_format_2_with_the_session_as_json() {
    let (_dir, store) = setup();
    let mut meta = session(WorkspaceId::new(), 0);
    meta.agent.kind = AgentKind::Shell;
    meta.agent.last_agent = Some(AgentKind::Codex);
    let mut snap = snapshot(&meta, 5);
    snap.format_version = 1;
    store.write_snapshot(&snap).unwrap();
    assert_eq!(format_on_disk(&store, meta.id), 2);
    let bytes = std::fs::read(store.paths().snapshot_file(&meta.id)).unwrap();
    let raw = zstd::stream::decode_all(&bytes[..]).unwrap();
    let file: FileV2 = postcard::from_bytes(&raw).unwrap();
    let json: serde_json::Value = serde_json::from_str(&file.session_json).unwrap();
    assert_eq!(json["agent"]["last_agent"], "Codex");
    let back = store.read_snapshot(meta.id).unwrap().unwrap();
    assert_eq!(
        back,
        SessionSnapshotFile {
            format_version: 2,
            ..snap
        }
    );
}

/// Why the session is JSON in format 2: a field missing from the file
/// (written before it existed) reads as its default, one the file has but
/// this build does not know (written by a newer one) is ignored. Neither
/// needs a new format version.
#[test]
fn session_fields_added_later_need_no_new_format() {
    let (_dir, store) = setup();
    let meta = session(WorkspaceId::new(), 0);
    let snap = snapshot(&meta, 3);
    let mut json = serde_json::to_value(&meta).unwrap();
    let agent = json["agent"].as_object_mut().unwrap();
    assert!(agent.remove("last_agent").is_some());
    agent.insert("from_a_newer_build".into(), 7.into());
    let file = FileV2Out {
        format_version: 2,
        saved_at_ms: snap.saved_at_ms,
        session_json: json.to_string(),
        styles: &snap.styles,
        history: &snap.history,
        screen: &snap.screen,
    };
    put_snapshot_file(&store, meta.id, &postcard::to_stdvec(&file).unwrap());
    let back = store.read_snapshot(meta.id).unwrap().unwrap();
    assert_eq!(back.session.agent.last_agent, None);
    assert_eq!(back, snap);
}

#[test]
fn snapshot_format_version_is_checked() {
    let (_dir, store) = setup();
    let meta = session(WorkspaceId::new(), 0);
    let snap = snapshot(&meta, 1);
    let file = FileV2Out {
        format_version: 2,
        saved_at_ms: snap.saved_at_ms,
        session_json: serde_json::to_string(&meta).unwrap(),
        styles: &snap.styles,
        history: &snap.history,
        screen: &snap.screen,
    };
    for found in [0, 3, 99] {
        let file = FileV2Out {
            format_version: found,
            session_json: file.session_json.clone(),
            ..file
        };
        put_snapshot_file(&store, meta.id, &postcard::to_stdvec(&file).unwrap());
        match store.read_snapshot(meta.id) {
            Err(StoreError::Format { found: f, want }) => {
                assert_eq!(f, found);
                assert_eq!(want, SNAPSHOT_FORMAT_VERSION);
            }
            other => panic!("expected format error for {found}, got {other:?}"),
        }
    }
    // A session that does not decode: an error quoting no value (serde_json
    // would quote the string here).
    let file = FileV2Out {
        session_json: "{\"cols\": \"secret-title\"}".into(),
        ..file
    };
    put_snapshot_file(&store, meta.id, &postcard::to_stdvec(&file).unwrap());
    let err = store.read_snapshot(meta.id).unwrap_err().to_string();
    assert!(err.contains("snapshot session"), "{err}");
    assert!(!err.contains("secret-title"), "{err}");
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

/// Review #14: files are removed before the rows, so a failing row
/// deletion never orphans a snapshot; the session stays listed for a retry.
#[test]
fn purge_removes_files_before_rows() {
    let (_dir, store) = setup();
    let meta = session(WorkspaceId::new(), 0);
    store.upsert_session(&meta).unwrap();
    store.write_snapshot(&snapshot(&meta, 10)).unwrap();
    store.db().execute_batch("DROP TABLE events").unwrap();
    assert!(store.purge_session(meta.id).is_err());
    assert!(!store.paths().snapshot_file(&meta.id).exists());
    assert!(store.get_session(meta.id).unwrap().is_some());
}

/// Review #14: a file that cannot be removed does not stop the purge.
#[test]
fn purge_deletes_rows_even_if_a_file_cannot_be_removed() {
    let (_dir, store) = setup();
    let meta = session(WorkspaceId::new(), 0);
    store.upsert_session(&meta).unwrap();
    let mut j = store.open_journal(meta.id).unwrap();
    j.append(1, b"hello").unwrap();
    j.flush().unwrap();
    drop(j);
    let journal = store.paths().journal_dir(&meta.id);
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = store.purge_session(meta.id);
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o700)).unwrap();
    result.unwrap();
    assert_eq!(store.get_session(meta.id).unwrap(), None);
    assert!(journal.exists(), "the undeletable journal was only logged");
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
