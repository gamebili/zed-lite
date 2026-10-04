use crate::{CELL_BYTES, PAGE_BYTES, PAGE_ROWS, PreviewPage, PreviewRequest};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{
    Connection, DatabaseName, OpenFlags,
    config::DbConfig,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::ValueRef,
};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const SCHEMA_BYTES: u64 = 512 * 1024;
const SCHEMA_OBJECTS: usize = 1000;
const SCHEMA_PAGES: usize = 512;
const PREVIEW_COLUMNS: usize = 64;
const QUERY_VALUE_BYTES: i32 = 64 * 1024;
const QUERY_OPERATIONS: usize = 2_000_000;
const QUERY_DURATION: Duration = Duration::from_secs(2);
const MAX_OFFSET: u64 = 1_000_000;
const SNAPSHOT_NOTE: &str = "Read-only immutable snapshot. Uncheckpointed WAL/journal changes are omitted. Each page is limited to 200 rows and 2 MiB; cells show bounded prefixes. Queries have a time and instruction budget.";

struct SchemaObject {
    kind: String,
    name: String,
    table: String,
    root_page: i64,
    definition: Option<String>,
}

struct Column {
    name: String,
    declaration: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key: i64,
    hidden: i64,
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

pub(crate) fn read(path: &Path, request: &PreviewRequest) -> Result<PreviewPage> {
    let started = Instant::now();
    ensure!(
        request.offset <= MAX_OFFSET,
        "SQLite preview offset exceeds the {MAX_OFFSET}-row scan budget"
    );
    let path = fs::canonicalize(path).context("Resolving SQLite file")?;
    let before = snapshot_stamps(&path)?;
    let encoding = inspect_schema_tree(&path, started)?;
    let mut uri = url::Url::from_file_path(&path)
        .map_err(|()| anyhow::anyhow!("Invalid SQLite file path"))?;
    uri.query_pairs_mut()
        .append_pair("mode", "ro")
        .append_pair("immutable", "1");
    let connection = Connection::open_with_flags(
        uri.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )
    .context("Opening immutable SQLite snapshot")?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, QUERY_VALUE_BYTES);
    connection.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, QUERY_VALUE_BYTES);
    connection.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, 100_000);
    connection.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 100);
    connection.set_limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT, 16);
    connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
    connection.set_limit(Limit::SQLITE_LIMIT_WORKER_THREADS, 0);
    connection.busy_timeout(Duration::ZERO)?;
    connection.execute_batch("PRAGMA query_only = ON; PRAGMA cache_size = -2048; PRAGMA mmap_size = 0; PRAGMA temp_store = FILE;")?;
    let mut operations = 0usize;
    connection.progress_handler(
        1000,
        Some(move || {
            operations = operations.saturating_add(1000);
            operations >= QUERY_OPERATIONS || started.elapsed() >= QUERY_DURATION
        }),
    );
    install_authorizer(&connection, HashSet::new());
    let objects = read_schema(&connection)?;
    let virtual_tables = objects
        .iter()
        .filter(|object| object.kind == "table" && object.root_page == 0)
        .map(|object| object.name.to_ascii_lowercase())
        .collect();
    install_authorizer(&connection, virtual_tables);

    let mut page = PreviewPage {
        title: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "SQLite database".into()),
        metadata: vec![
            ("Format".into(), "SQLite".into()),
            ("Mode".into(), "Read-only immutable".into()),
            ("Schema objects".into(), objects.len().to_string()),
        ],
        sections: vec!["Schema".into()],
        note: Some(SNAPSHOT_NOTE.into()),
        ..PreviewPage::default()
    };
    for object in objects
        .iter()
        .filter(|object| matches!(object.kind.as_str(), "table" | "view"))
    {
        page.sections
            .push(format!("{}:{}", object.kind, object.name));
        page.sections.push(format!("columns:{}", object.name));
        page.sections.push(format!("indexes:{}", object.name));
        page.sections.push(format!("triggers:{}", object.name));
    }
    let selected = request
        .section
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| "Schema".into());
    ensure!(
        page.sections.iter().any(|section| section == &selected),
        "Unknown SQLite preview section"
    );
    let result = if selected == "Schema" {
        read_schema_page(&objects, request.offset, &mut page)
    } else {
        let (kind, name) = selected.split_once(':').context("Invalid SQLite section")?;
        let object = objects
            .iter()
            .find(|object| object.name == name && matches!(object.kind.as_str(), "table" | "view"))
            .context("SQLite table or view no longer exists")?;
        match kind {
            "columns" => read_column_page(&connection, object, request.offset, &mut page),
            "indexes" | "triggers" => read_related_schema_page(
                &connection,
                &objects,
                object,
                kind,
                request.offset,
                &mut page,
            ),
            "table" | "view" => read_rows(
                &connection,
                object,
                request.offset,
                encoding,
                started,
                &mut page,
            ),
            _ => bail!("Unknown SQLite preview section"),
        }
    };
    if let Err(error) = result {
        append_note(
            &mut page,
            &format!("Content preview stopped: {error:#}. Schema remains available."),
        );
    }
    ensure!(
        before == snapshot_stamps(&path)?,
        "SQLite database or sidecars changed during preview; reload after writes finish"
    );
    Ok(page)
}

fn install_authorizer(connection: &Connection, virtual_tables: HashSet<String>) {
    connection.authorizer(Some(move |context: AuthContext<'_>| match context.action {
        AuthAction::Select => Authorization::Allow,
        AuthAction::Read { table_name, .. }
            if !virtual_tables.contains(&table_name.to_ascii_lowercase()) =>
        {
            Authorization::Allow
        }
        AuthAction::Pragma { pragma_name, .. }
            if matches!(
                pragma_name.to_ascii_lowercase().as_str(),
                "table_xinfo" | "index_list" | "index_xinfo"
            ) =>
        {
            Authorization::Allow
        }
        AuthAction::Function { function_name }
            if matches!(
                function_name.to_ascii_lowercase().as_str(),
                "typeof"
                    | "substr"
                    | "substring"
                    | "hex"
                    | "length"
                    | "coalesce"
                    | "ifnull"
                    | "nullif"
                    | "lower"
                    | "upper"
                    | "trim"
                    | "ltrim"
                    | "rtrim"
                    | "abs"
                    | "round"
                    | "quote"
                    | "instr"
            ) =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }));
}

fn read_schema(connection: &Connection) -> Result<Vec<SchemaObject>> {
    let mut statement = connection
        .prepare("SELECT type, name, tbl_name, rootpage, sql FROM main.sqlite_schema LIMIT ?1")?;
    let mut rows = statement.query([SCHEMA_OBJECTS as i64 + 1])?;
    let mut objects = Vec::new();
    while let Some(row) = rows.next()? {
        ensure!(
            objects.len() < SCHEMA_OBJECTS,
            "SQLite schema exceeds the object budget"
        );
        objects.push(SchemaObject {
            kind: row.get(0)?,
            name: row.get(1)?,
            table: row.get(2)?,
            root_page: row.get(3)?,
            definition: row.get(4)?,
        });
    }
    Ok(objects)
}

fn read_schema_page(objects: &[SchemaObject], offset: u64, page: &mut PreviewPage) -> Result<()> {
    page.columns = ["Type", "Name", "Table", "Root page", "Definition"]
        .map(str::to_owned)
        .to_vec();
    paginate(
        objects.iter().skip(offset as usize).map(|object| {
            vec![
                object.kind.clone(),
                limited(&object.name),
                limited(&object.table),
                object.root_page.to_string(),
                limited(
                    object
                        .definition
                        .as_deref()
                        .unwrap_or("Automatic index; no explicit DDL"),
                ),
            ]
        }),
        offset,
        page,
    );
    Ok(())
}

fn columns(connection: &Connection, object: &SchemaObject) -> Result<Vec<Column>> {
    ensure!(
        object.kind != "table" || object.root_page > 0,
        "Virtual-table modules are not executed by the read-only preview; inspect their CREATE statement in Schema"
    );
    let mut statement = connection.prepare(&format!(
        "PRAGMA main.table_xinfo({})",
        identifier(&object.name)
    ))?;
    let mut rows = statement.query([])?;
    let mut columns = Vec::new();
    while let Some(row) = rows.next()? {
        ensure!(
            columns.len() < 2048,
            "SQLite table exceeds the column budget"
        );
        columns.push(Column {
            name: row.get(1)?,
            declaration: row.get(2)?,
            not_null: row.get(3)?,
            default_value: row.get(4)?,
            primary_key: row.get(5)?,
            hidden: row.get(6)?,
        });
    }
    Ok(columns)
}

fn read_column_page(
    connection: &Connection,
    object: &SchemaObject,
    offset: u64,
    page: &mut PreviewPage,
) -> Result<()> {
    let columns = columns(connection, object)?;
    page.columns = [
        "Column",
        "Declared type",
        "Not null",
        "Default",
        "Primary key position",
        "Storage",
    ]
    .map(str::to_owned)
    .to_vec();
    paginate(
        columns.iter().skip(offset as usize).map(|column| {
            vec![
                limited(&column.name),
                limited(&column.declaration),
                column.not_null.to_string(),
                limited(column.default_value.as_deref().unwrap_or("—")),
                column.primary_key.to_string(),
                match column.hidden {
                    0 => "Ordinary",
                    1 => "Hidden",
                    2 => "Generated virtual",
                    3 => "Generated stored",
                    _ => "Unknown",
                }
                .into(),
            ]
        }),
        offset,
        page,
    );
    Ok(())
}

fn read_related_schema_page(
    connection: &Connection,
    objects: &[SchemaObject],
    object: &SchemaObject,
    kind: &str,
    offset: u64,
    page: &mut PreviewPage,
) -> Result<()> {
    if kind == "triggers" {
        page.columns = ["Trigger", "Table", "Definition"]
            .map(str::to_owned)
            .to_vec();
        paginate(
            objects
                .iter()
                .filter(|candidate| candidate.kind == "trigger" && candidate.table == object.name)
                .skip(offset as usize)
                .map(|candidate| {
                    vec![
                        limited(&candidate.name),
                        limited(&candidate.table),
                        limited(candidate.definition.as_deref().unwrap_or("—")),
                    ]
                }),
            offset,
            page,
        );
        return Ok(());
    }
    ensure!(
        object.kind != "table" || object.root_page > 0,
        "Virtual-table modules are not executed; inspect Schema"
    );
    page.columns = [
        "Index",
        "Unique",
        "Origin",
        "Partial",
        "Key columns",
        "Definition",
    ]
    .map(str::to_owned)
    .to_vec();
    let mut statement = connection.prepare(&format!(
        "PRAGMA main.index_list({})",
        identifier(&object.name)
    ))?;
    let mut rows = statement.query([])?;
    let mut indexes_seen = 0u64;
    let mut page_bytes = 0usize;
    while let Some(row) = rows.next()? {
        ensure!(
            indexes_seen < SCHEMA_OBJECTS as u64,
            "SQLite index list exceeds the object budget"
        );
        indexes_seen += 1;
        if indexes_seen <= offset {
            continue;
        }
        if page.rows.len() == PAGE_ROWS {
            page.next_offset = Some(offset + page.rows.len() as u64);
            break;
        }
        let name: String = row.get(1)?;
        let mut details =
            connection.prepare(&format!("PRAGMA main.index_xinfo({})", identifier(&name)))?;
        let mut details_rows = details.query([])?;
        let mut keys = String::new();
        while let Some(detail) = details_rows.next()? {
            let key: bool = detail.get(5)?;
            if key && keys.len() < CELL_BYTES {
                if !keys.is_empty() {
                    keys.push_str(", ");
                }
                let column: Option<String> = detail.get(2)?;
                let descending: bool = detail.get(3)?;
                let collation: String = detail.get(4)?;
                keys.push_str(&limited(column.as_deref().unwrap_or("(expression)")));
                if descending {
                    keys.push_str(" DESC");
                }
                keys.push_str(" COLLATE ");
                keys.push_str(&limited(&collation));
            }
        }
        let definition = objects
            .iter()
            .find(|candidate| candidate.kind == "index" && candidate.name == name)
            .and_then(|candidate| candidate.definition.as_deref())
            .unwrap_or("Automatic primary-key/unique index");
        let index = vec![
            limited(&name),
            row.get::<_, bool>(2)?.to_string(),
            row.get::<_, String>(3)?,
            row.get::<_, bool>(4)?.to_string(),
            limited(&keys),
            limited(definition),
        ];
        let row_bytes = index
            .iter()
            .map(|value| value.len() + std::mem::size_of::<String>())
            .sum::<usize>();
        if page_bytes + row_bytes > PAGE_BYTES {
            page.next_offset = Some(offset + page.rows.len() as u64);
            break;
        }
        page_bytes += row_bytes;
        page.rows.push(index);
    }
    Ok(())
}

fn read_rows(
    connection: &Connection,
    object: &SchemaObject,
    offset: u64,
    encoding: u32,
    started: Instant,
    page: &mut PreviewPage,
) -> Result<()> {
    let columns = columns(connection, object)?;
    let visible_columns: Vec<&Column> = columns
        .iter()
        .filter(|column| column.hidden != 1)
        .take(PREVIEW_COLUMNS)
        .collect();
    ensure!(!visible_columns.is_empty(), "No readable SQLite columns");
    page.columns = visible_columns
        .iter()
        .map(|column| limited(&column.name))
        .collect();
    if columns.len() > visible_columns.len() {
        append_note(
            page,
            "This row page shows the first 64 columns. The Columns section exposes all column definitions.",
        );
    }
    let table = identifier(&object.name);
    let row_id = if object.kind == "table" && object.root_page > 0 {
        ["_rowid_", "rowid", "oid"]
            .into_iter()
            .find(|alias| {
                !columns
                    .iter()
                    .any(|column| column.name.eq_ignore_ascii_case(alias))
            })
            .map(|alias| format!("{table}.{}", identifier(alias)))
            .filter(|selector| {
                connection
                    .prepare(&format!("SELECT {selector} FROM main.{table} LIMIT 0"))
                    .is_ok()
            })
    } else {
        None
    };
    if row_id.is_none() {
        append_note(
            page,
            "Views and WITHOUT ROWID tables use bounded SQL values; cells larger than 64 KiB stop this page. Virtual tables and non-allowlisted SQL functions are never executed.",
        );
    }
    // typeof(column) lets SQLite inspect the record header without loading a large
    // overflow value. Incremental BLOB I/O then reads only the displayed prefix.
    let mut projections = vec![row_id.clone().unwrap_or_else(|| "NULL".into())];
    for column in &visible_columns {
        let name = format!("{table}.{}", identifier(&column.name));
        projections.push(format!("typeof({name})"));
        if row_id.is_some() {
            projections.push(format!("CASE typeof({name}) WHEN 'integer' THEN {name} WHEN 'real' THEN {name} ELSE NULL END"));
        } else {
            projections.push(format!("CASE typeof({name}) WHEN 'text' THEN substr({name}, 1, {CELL_BYTES}) WHEN 'blob' THEN substr({name}, 1, {}) ELSE {name} END", (CELL_BYTES - 128) / 2));
        }
    }
    let sql = format!(
        "SELECT {} FROM main.{table} LIMIT ?1 OFFSET ?2",
        projections.join(", ")
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([PAGE_ROWS as i64 + 1, offset as i64])?;
    let mut page_bytes = 0usize;
    while let Some(row) = rows.next()? {
        ensure!(
            started.elapsed() < QUERY_DURATION,
            "SQLite query exceeded the time budget"
        );
        if page.rows.len() == PAGE_ROWS {
            page.next_offset = Some(offset + page.rows.len() as u64);
            break;
        }
        let current_row_id: Option<i64> = row.get(0)?;
        let mut values = Vec::with_capacity(visible_columns.len());
        for (position, column) in visible_columns.iter().enumerate() {
            let value_type: String = row.get(1 + position * 2)?;
            let value_position = 2 + position * 2;
            let value = match (value_type.as_str(), current_row_id) {
                ("text" | "blob", Some(current_row_id)) => match read_incremental_value(
                    connection,
                    object,
                    column,
                    current_row_id,
                    &value_type,
                    encoding,
                ) {
                    Ok(value) => value,
                    Err(error) => {
                        append_note(
                            page,
                            &format!("Some cell prefixes are unavailable: {error}."),
                        );
                        format!("{} (prefix unavailable)", value_type.to_ascii_uppercase())
                    }
                },
                _ => format_value(row.get_ref(value_position)?),
            };
            values.push(value);
        }
        let row_bytes = values
            .iter()
            .map(|value| value.len() + std::mem::size_of::<String>())
            .sum::<usize>();
        if page_bytes + row_bytes > PAGE_BYTES {
            page.next_offset = Some(offset + page.rows.len() as u64);
            break;
        }
        page_bytes += row_bytes;
        page.rows.push(values);
    }
    Ok(())
}

fn read_incremental_value(
    connection: &Connection,
    object: &SchemaObject,
    column: &Column,
    row_id: i64,
    value_type: &str,
    encoding: u32,
) -> Result<String> {
    let blob =
        connection.blob_open(DatabaseName::Main, &object.name, &column.name, row_id, true)?;
    let length = blob.len();
    let prefix_limit = if value_type == "blob" {
        (CELL_BYTES - 128) / 2
    } else {
        CELL_BYTES
    };
    let mut prefix = vec![0; length.min(prefix_limit)];
    blob.read_at_exact(&mut prefix, 0)?;
    blob.close()?;
    let suffix = if length > prefix.len() {
        " … (truncated)"
    } else {
        ""
    };
    let value = if value_type == "blob" {
        format!("BLOB ({length} bytes): {}{suffix}", hex_prefix(&prefix))
    } else {
        let text = match encoding {
            2 | 3 => String::from_utf16_lossy(
                &prefix
                    .chunks_exact(2)
                    .map(|pair| {
                        if encoding == 2 {
                            u16::from_le_bytes([pair[0], pair[1]])
                        } else {
                            u16::from_be_bytes([pair[0], pair[1]])
                        }
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => String::from_utf8_lossy(&prefix).into_owned(),
        };
        format!(
            "TEXT ({length} bytes) {}{suffix}",
            serde_json::to_string(&text)?
        )
    };
    Ok(limited(&value))
}

fn format_value(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(value) => format!("INTEGER {value}"),
        ValueRef::Real(value) => format!("REAL {value}"),
        ValueRef::Text(value) => limited(&format!(
            "TEXT {}",
            serde_json::to_string(&String::from_utf8_lossy(value))
                .unwrap_or_else(|error| format!("(display error: {error})"))
        )),
        ValueRef::Blob(value) => limited(&format!("BLOB prefix: {}", hex_prefix(value))),
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    output
}

fn paginate(rows: impl Iterator<Item = Vec<String>>, offset: u64, page: &mut PreviewPage) {
    let mut page_bytes = 0usize;
    for row in rows {
        let row_bytes = row
            .iter()
            .map(|cell| cell.len() + std::mem::size_of::<String>())
            .sum::<usize>();
        if page.rows.len() == PAGE_ROWS || page_bytes + row_bytes > PAGE_BYTES {
            page.next_offset = Some(offset + page.rows.len() as u64);
            break;
        }
        page_bytes += row_bytes;
        page.rows.push(row);
    }
}

fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn limited(value: &str) -> String {
    if value.len() <= CELL_BYTES {
        return value.into();
    }
    let mut end = CELL_BYTES - " … (truncated)".len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … (truncated)", &value[..end])
}

fn append_note(page: &mut PreviewPage, message: &str) {
    let note = page.note.get_or_insert_with(String::new);
    if !note.contains(message) && note.len() + message.len() + 1 < CELL_BYTES {
        if !note.is_empty() {
            note.push(' ');
        }
        note.push_str(message);
    }
}

fn snapshot_stamps(path: &Path) -> Result<Vec<Option<FileStamp>>> {
    let mut paths = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        paths.push(PathBuf::from(name));
    }
    paths
        .iter()
        .map(|path| match fs::metadata(path) {
            Ok(metadata) => {
                #[cfg(unix)]
                use std::os::unix::fs::MetadataExt as _;
                Ok(Some(FileStamp {
                    size: metadata.len(),
                    modified: metadata.modified()?,
                    #[cfg(unix)]
                    identity: (
                        metadata.dev(),
                        metadata.ino(),
                        metadata.ctime(),
                        metadata.ctime_nsec(),
                    ),
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("Inspecting SQLite file {}", path.display()))
            }
        })
        .collect()
}

// SQLite loads and parses its entire schema before ordinary queries run. Walk
// only the sqlite_schema B-tree first to reject oversized schemas before that
// allocation, without reading table data or overflow payloads into memory.
fn inspect_schema_tree(path: &Path, started: Instant) -> Result<u32> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut header = [0u8; 100];
    file.read_exact(&mut header)
        .context("Reading SQLite header")?;
    ensure!(
        header.starts_with(b"SQLite format 3\0"),
        "Invalid SQLite file header"
    );
    let raw_page_size = u16::from_be_bytes([header[16], header[17]]);
    let page_size = if raw_page_size == 1 {
        65536
    } else {
        usize::from(raw_page_size)
    };
    ensure!(
        (512..=65536).contains(&page_size) && page_size.is_power_of_two(),
        "Invalid SQLite page size"
    );
    let mut pending = vec![1u32];
    let mut visited = HashSet::new();
    let mut page = vec![0u8; page_size];
    let mut schema_bytes = 0u64;
    let mut schema_objects = 0usize;
    while let Some(page_number) = pending.pop() {
        ensure!(
            started.elapsed() < QUERY_DURATION,
            "SQLite schema inspection exceeded the time budget"
        );
        ensure!(
            visited.len() < SCHEMA_PAGES && visited.insert(page_number),
            "SQLite schema exceeds the page budget or has cyclic pages"
        );
        let offset = u64::from(page_number)
            .checked_sub(1)
            .and_then(|number| number.checked_mul(page_size as u64))
            .context("Invalid SQLite schema page number")?;
        ensure!(
            offset
                .checked_add(page_size as u64)
                .is_some_and(|end| end <= file_size),
            "SQLite schema page exceeds the file size"
        );
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut page)?;
        let header_offset = if page_number == 1 { 100 } else { 0 };
        let page_type = *page
            .get(header_offset)
            .context("Invalid SQLite B-tree header")?;
        ensure!(
            matches!(page_type, 5 | 13),
            "Invalid SQLite schema B-tree page"
        );
        let cells = read_u16(&page, header_offset + 3)? as usize;
        let pointer_offset = header_offset + if page_type == 5 { 12 } else { 8 };
        ensure!(
            pointer_offset + cells * 2 <= page_size,
            "Invalid SQLite schema cell pointers"
        );
        if page_type == 5 {
            ensure!(
                pending.len() + cells < SCHEMA_PAGES,
                "SQLite schema exceeds the page budget"
            );
            pending.push(read_u32(&page, header_offset + 8)?);
        }
        for position in 0..cells {
            let cell_offset = read_u16(&page, pointer_offset + position * 2)? as usize;
            if page_type == 5 {
                pending.push(read_u32(&page, cell_offset)?);
            } else {
                let (payload_length, _) = read_varint(&page, cell_offset)?;
                schema_bytes = schema_bytes
                    .checked_add(payload_length)
                    .context("SQLite schema size overflow")?;
                schema_objects += 1;
                ensure!(
                    schema_bytes <= SCHEMA_BYTES && schema_objects <= SCHEMA_OBJECTS,
                    "SQLite schema exceeds the bounded preview budget ({SCHEMA_BYTES} bytes, {SCHEMA_OBJECTS} objects)"
                );
            }
        }
    }
    let encoding = u32::from_be_bytes([header[56], header[57], header[58], header[59]]);
    ensure!(matches!(encoding, 0..=3), "Invalid SQLite text encoding");
    Ok(encoding)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let bytes = bytes
        .get(offset..offset.saturating_add(2))
        .context("Truncated SQLite page")?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let bytes = bytes
        .get(offset..offset.saturating_add(4))
        .context("Truncated SQLite page")?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_varint(bytes: &[u8], offset: usize) -> Result<(u64, usize)> {
    let mut value = 0u64;
    for position in 0..9 {
        let byte = *bytes
            .get(offset.saturating_add(position))
            .context("Truncated SQLite variable integer")?;
        if position == 8 {
            return Ok(((value << 8) | u64::from(byte), 9));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok((value, position + 1));
        }
    }
    bail!("Invalid SQLite variable integer")
}

#[cfg(test)]
#[path = "sqlite_tests.rs"]
mod tests;
