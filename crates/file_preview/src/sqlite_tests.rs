use super::*;
use std::io::Write as _;
use tempfile::TempDir;

fn database() -> Result<(TempDir, PathBuf, Connection)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("preview ? # 数据.sqlite3");
    let connection = Connection::open(&path)?;
    Ok((directory, path, connection))
}

fn section(path: &Path, name: &str, offset: u64) -> Result<PreviewPage> {
    read(
        path,
        &PreviewRequest {
            section: Some(name.into()),
            offset,
        },
    )
}

#[test]
fn previews_rows_schema_columns_indexes_triggers_and_views() -> Result<()> {
    let (_directory, path, connection) = database()?;
    connection.execute_batch(
        "CREATE TABLE data (id INTEGER PRIMARY KEY, nullable TEXT, text_value TEXT DEFAULT 'default', payload BLOB, amount REAL);
         CREATE INDEX data_text ON data(text_value COLLATE NOCASE DESC);
         CREATE TRIGGER data_insert AFTER INSERT ON data BEGIN UPDATE data SET amount = amount WHERE id = NEW.id; END;
         CREATE VIEW data_view AS SELECT id, upper(text_value) AS upper_text FROM data;
         INSERT INTO data VALUES (1, NULL, 'NULL', X'00ABFF', 2.5);",
    )?;
    drop(connection);
    let before = fs::read(&path)?;
    let page = section(&path, "table:data", 0)?;
    assert_eq!(
        page.columns,
        ["id", "nullable", "text_value", "payload", "amount"]
    );
    let row = page.rows.first().context("Missing data row")?;
    assert_eq!(row.first().map(String::as_str), Some("INTEGER 1"));
    assert_eq!(row.get(1).map(String::as_str), Some("NULL"));
    assert_eq!(
        row.get(2).map(String::as_str),
        Some("TEXT (4 bytes) \"NULL\"")
    );
    assert_eq!(
        row.get(3).map(String::as_str),
        Some("BLOB (3 bytes): 00ABFF")
    );
    assert_eq!(row.get(4).map(String::as_str), Some("REAL 2.5"));
    assert!(page.sections.contains(&"view:data_view".into()));
    let schema = read(&path, &PreviewRequest::default())?;
    assert!(
        schema
            .rows
            .iter()
            .any(|row| row.iter().any(|cell| cell.contains("CREATE TABLE data")))
    );
    let columns = section(&path, "columns:data", 0)?;
    assert!(
        columns
            .rows
            .iter()
            .any(|row| row.first().is_some_and(|name| name == "text_value")
                && row.get(3).is_some_and(|value| value == "'default'"))
    );
    let indexes = section(&path, "indexes:data", 0)?;
    assert!(indexes.rows.iter().any(|row| {
        row.iter()
            .any(|cell| cell.contains("text_value DESC COLLATE NOCASE"))
    }));
    let triggers = section(&path, "triggers:data", 0)?;
    assert!(triggers.rows.iter().any(|row| {
        row.iter()
            .any(|cell| cell.contains("CREATE TRIGGER data_insert"))
    }));
    let view = section(&path, "view:data_view", 0)?;
    assert_eq!(view.rows.len(), 1);
    assert!(
        view.rows
            .first()
            .and_then(|row| row.get(1))
            .is_some_and(|value| value == "TEXT \"NULL\"")
    );
    assert_eq!(before, fs::read(&path)?);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        assert!(!Path::new(&sidecar).exists());
    }
    Ok(())
}

#[test]
fn pages_actual_rows_and_quotes_database_identifiers() -> Result<()> {
    let (_directory, path, mut connection) = database()?;
    let table_name = "rows\"; DROP TABLE victim; --";
    let column_name = "column\" with space";
    connection.execute_batch(&format!(
        "CREATE TABLE {} ({} INTEGER); CREATE TABLE victim (value TEXT);",
        identifier(table_name),
        identifier(column_name)
    ))?;
    let transaction = connection.transaction()?;
    {
        let mut insert = transaction.prepare(&format!(
            "INSERT INTO {} VALUES (?1)",
            identifier(table_name)
        ))?;
        for value in 0..450 {
            insert.execute([value])?;
        }
    }
    transaction.commit()?;
    drop(connection);
    let first = section(&path, &format!("table:{table_name}"), 0)?;
    assert_eq!(first.rows.len(), PAGE_ROWS);
    assert_eq!(first.next_offset, Some(200));
    let second = section(&path, &format!("table:{table_name}"), 200)?;
    assert_eq!(
        second
            .rows
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("INTEGER 200")
    );
    assert_eq!(second.next_offset, Some(400));
    let last = section(&path, &format!("table:{table_name}"), 400)?;
    assert_eq!(last.rows.len(), 50);
    assert_eq!(last.next_offset, None);
    assert!(section(&path, "table:victim; DROP TABLE victim", 0).is_err());
    assert!(section(&path, "table:victim", 0).is_ok());
    Ok(())
}

#[test]
fn reads_bounded_prefixes_of_large_text_and_blobs() -> Result<()> {
    let (_directory, path, connection) = database()?;
    connection.execute_batch(
        "CREATE TABLE data (payload BLOB, text_value TEXT);
         INSERT INTO data VALUES (zeroblob(48 * 1024 * 1024), printf('%.*c', 4 * 1024 * 1024, 'x'));",
    )?;
    drop(connection);
    let page = section(&path, "table:data", 0)?;
    let row = page
        .rows
        .first()
        .context("Large cells should remain previewable")?;
    assert!(row.first().is_some_and(
        |value| value.starts_with("BLOB (50331648 bytes): 0000") && value.contains("truncated")
    ));
    assert!(row.get(1).is_some_and(
        |value| value.starts_with("TEXT (4194304 bytes) \"xxxx") && value.contains("truncated")
    ));
    assert!(row.iter().all(|value| value.len() <= CELL_BYTES));
    Ok(())
}

#[test]
fn caps_page_output_and_continues_at_the_first_omitted_row() -> Result<()> {
    let (_directory, path, connection) = database()?;
    let declarations = (0..8)
        .map(|position| format!("text_{position} TEXT"))
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute_batch(&format!("CREATE TABLE data ({declarations});"))?;
    let values = (0..8)
        .map(|_| "printf('%.*c', 6000, 'x')")
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute_batch(&format!("WITH RECURSIVE numbers(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM numbers WHERE value < 200) INSERT INTO data SELECT {values} FROM numbers;"))?;
    drop(connection);
    let page = section(&path, "table:data", 0)?;
    assert!(!page.rows.is_empty());
    assert!(page.rows.len() < PAGE_ROWS);
    assert!(
        page.rows
            .iter()
            .flatten()
            .map(|value| value.len() + std::mem::size_of::<String>())
            .sum::<usize>()
            <= PAGE_BYTES
    );
    assert_eq!(page.next_offset, Some(page.rows.len() as u64));
    let next = section(
        &path,
        "table:data",
        page.next_offset.context("Missing continuation")?,
    )?;
    assert!(!next.rows.is_empty());
    Ok(())
}

#[test]
fn reads_without_rowid_tables_and_limits_oversized_sql_values() -> Result<()> {
    let (_directory, path, connection) = database()?;
    connection.execute_batch(
        "CREATE TABLE data (id INTEGER PRIMARY KEY, value TEXT) WITHOUT ROWID;
         INSERT INTO data VALUES (1, 'first'), (2, printf('%.*c', 256 * 1024, 'x'));",
    )?;
    drop(connection);
    let page = section(&path, "table:data", 0)?;
    assert_eq!(page.rows.len(), 1);
    assert_eq!(
        page.rows
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("INTEGER 1")
    );
    assert!(page.note.as_deref().is_some_and(|note| note.contains("Content preview stopped") && note.contains("64 KiB")));
    let later = section(&path, "table:data", 2)?;
    assert!(later.rows.is_empty());
    Ok(())
}

#[test]
fn immutable_snapshot_omits_wal_and_does_not_change_sidecars() -> Result<()> {
    let (_directory, path, connection) = database()?;
    connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data (id INTEGER); INSERT INTO data VALUES (1); PRAGMA wal_checkpoint(TRUNCATE); INSERT INTO data VALUES (2);")?;
    let mut paths = vec![path.clone()];
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        paths.push(PathBuf::from(name));
    }
    let before: Vec<Vec<u8>> = paths.iter().map(fs::read).collect::<std::io::Result<_>>()?;
    let stamps = snapshot_stamps(&path)?;
    let page = section(&path, "table:data", 0)?;
    assert_eq!(page.rows.len(), 1);
    assert!(
        page.note
            .as_deref()
            .is_some_and(|note| note.contains("Uncheckpointed WAL"))
    );
    let after: Vec<Vec<u8>> = paths.iter().map(fs::read).collect::<std::io::Result<_>>()?;
    assert_eq!(before, after);
    assert_eq!(stamps, snapshot_stamps(&path)?);
    drop(connection);
    Ok(())
}

#[test]
fn does_not_execute_virtual_tables_or_non_allowlisted_view_functions() -> Result<()> {
    let (_directory, path, connection) = database()?;
    connection.execute_batch("CREATE TABLE data (value TEXT); CREATE VIRTUAL TABLE search USING fts5(content); CREATE VIEW dangerous AS SELECT randomblob(1000000000) AS value;")?;
    drop(connection);
    let virtual_table = section(&path, "table:search", 0)?;
    assert!(virtual_table.rows.is_empty());
    assert!(
        virtual_table
            .note
            .as_deref()
            .is_some_and(|note| note.contains("Virtual-table modules are not executed"))
    );
    let dangerous = section(&path, "view:dangerous", 0)?;
    assert!(dangerous.rows.is_empty());
    assert!(
        dangerous
            .note
            .as_deref()
            .is_some_and(|note| note.contains("Content preview stopped"))
    );
    assert!(
        section(&path, "Schema", 0)?
            .rows
            .iter()
            .any(|row| row.iter().any(|cell| cell.contains("randomblob")))
    );
    Ok(())
}

#[test]
fn stops_expensive_views_and_rejects_extreme_offsets() -> Result<()> {
    let (_directory, path, connection) = database()?;
    connection.execute_batch(
        "CREATE TABLE data (id INTEGER);
         WITH RECURSIVE numbers(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM numbers WHERE value < 500) INSERT INTO data SELECT value FROM numbers;
         CREATE VIEW expensive AS SELECT a.id AS value FROM data a, data b, data c ORDER BY b.id DESC, c.id DESC;",
    )?;
    drop(connection);
    let started = Instant::now();
    let page = section(&path, "view:expensive", 0)?;
    assert!(page.rows.is_empty());
    assert!(
        page.note
            .as_deref()
            .is_some_and(|note| note.contains("Content preview stopped"))
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(section(&path, "table:data", u64::MAX).is_err());
    Ok(())
}

#[test]
fn rejects_oversized_schema_before_sqlite_parses_it() -> Result<()> {
    let (_directory, path, connection) = database()?;
    let default = "x".repeat(SCHEMA_BYTES as usize + 1);
    connection.execute_batch(&format!(
        "CREATE TABLE oversized (value TEXT DEFAULT '{default}')"
    ))?;
    drop(connection);
    let error = read(&path, &PreviewRequest::default())
        .err()
        .context("Oversized schema should be rejected")?;
    assert!(
        error
            .to_string()
            .contains("schema exceeds the bounded preview budget")
    );
    Ok(())
}

#[test]
fn opens_a_sparse_eight_gibibyte_database_without_reading_the_file() -> Result<()> {
    let (_directory, path, connection) = database()?;
    connection.execute_batch("CREATE TABLE data (id INTEGER); INSERT INTO data VALUES (42);")?;
    drop(connection);
    let mut file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
    let mut header = [0u8; 100];
    file.read_exact(&mut header)?;
    let raw_page_size = u16::from_be_bytes([header[16], header[17]]);
    let page_size = if raw_page_size == 1 {
        65536
    } else {
        u64::from(raw_page_size)
    };
    let size = 8 * 1024 * 1024 * 1024u64;
    file.set_len(size)?;
    file.seek(SeekFrom::Start(28))?;
    file.write_all(&u32::try_from(size / page_size)?.to_be_bytes())?;
    file.sync_all()?;
    drop(file);
    let page = section(&path, "table:data", 0)?;
    assert_eq!(
        page.rows
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("INTEGER 42")
    );
    assert_eq!(fs::metadata(&path)?.len(), size);
    Ok(())
}

#[test]
fn authorizer_denies_writes_attaches_and_extension_calls() -> Result<()> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch("CREATE TABLE data (id INTEGER)")?;
    install_authorizer(&connection, HashSet::new());
    for sql in [
        "INSERT INTO data VALUES (1)",
        "DELETE FROM data",
        "DROP TABLE data",
        "ATTACH DATABASE ':memory:' AS other",
        "SELECT load_extension('untrusted')",
        "PRAGMA writable_schema=ON",
    ] {
        assert!(
            connection.execute_batch(sql).is_err(),
            "Unexpectedly allowed {sql}"
        );
    }
    Ok(())
}
