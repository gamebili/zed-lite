use super::*;
use anyhow::Result;
use std::{fs, io::Cursor};

fn property<'a>(summary: &'a BinarySummary, label: &str) -> Option<&'a str> {
    summary
        .props
        .iter()
        .find(|property| property.label == label)
        .map(|property| property.value.as_str())
}

fn entries<'a>(summary: &'a BinarySummary, title: &str) -> &'a [String] {
    summary
        .sections
        .iter()
        .find(|section| section.title == title)
        .map(|section| section.items.as_slice())
        .unwrap_or(&[])
}

fn int(out: &mut Vec<u8>, n: i32) {
    out.extend(n.to_le_bytes());
}
fn long(out: &mut Vec<u8>, n: i64) {
    out.extend(n.to_le_bytes());
}
fn string(out: &mut Vec<u8>, s: &str) {
    int(out, s.len() as i32 + 1);
    out.extend(s.as_bytes());
    out.push(0);
}
fn wide(out: &mut Vec<u8>, s: &str) {
    let units: Vec<_> = s.encode_utf16().chain([0]).collect();
    int(out, -(units.len() as i32));
    for unit in units {
        out.extend(unit.to_le_bytes());
    }
}
fn name(out: &mut Vec<u8>, n: i32) {
    int(out, n);
    int(out, 0);
}
fn patch(out: &mut [u8], at: usize, n: i32) {
    out[at..at + 4].copy_from_slice(&n.to_le_bytes());
}
fn slot(out: &mut Vec<u8>) -> usize {
    let at = out.len();
    int(out, 0);
    at
}
fn engine(out: &mut Vec<u8>) {
    out.extend(5u16.to_le_bytes());
    out.extend(6u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    int(out, 123);
    string(out, "test-engine");
}
struct Fixture {
    bytes: Vec<u8>,
    name_count: usize,
    export_offset: usize,
    registry_slot: usize,
    name_offset: usize,
}
fn fixture(ue5: i32, jb: Option<i32>, bp: bool) -> Fixture {
    let mut b = Vec::new();
    int(&mut b, 0x9e2a83c1u32 as i32);
    int(
        &mut b,
        if ue5 >= 1016 {
            -9
        } else if ue5 > 0 {
            -8
        } else {
            -7
        },
    );
    int(&mut b, 0);
    int(&mut b, 522);
    if ue5 > 0 {
        int(&mut b, ue5);
    }
    int(&mut b, 0);
    let total = if ue5 >= 1016 {
        b.extend([0x42; 20]);
        slot(&mut b)
    } else {
        0
    };
    int(&mut b, 0);
    let total = if ue5 < 1016 {
        if let Some(marker) = jb {
            int(&mut b, marker);
        }
        slot(&mut b)
    } else {
        total
    };
    string(&mut b, "/Game/Blueprints");
    int(&mut b, 0x00200000);
    let name_count = slot(&mut b);
    let name_offset = slot(&mut b);
    if ue5 >= 1008 {
        b.extend([0; 8]);
    }
    string(&mut b, "");
    b.extend([0; 8]);
    int(&mut b, 5);
    let export_offset_slot = slot(&mut b);
    int(&mut b, 8);
    let import_offset_slot = slot(&mut b);
    if ue5 >= 1015 {
        b.extend([0; 16]);
    }
    if ue5 >= 1014 {
        int(&mut b, 0);
    }
    int(&mut b, 0);
    int(&mut b, 0);
    int(&mut b, 0);
    int(&mut b, 0);
    int(&mut b, 0);
    if ue5 < 1016 {
        b.extend([0; 16]);
    }
    if jb.is_none() || jb == Some(-265535) {
        b.extend([0; 16]);
    }
    int(&mut b, 0);
    engine(&mut b);
    engine(&mut b);
    int(&mut b, 0);
    int(&mut b, 0);
    int(&mut b, 0);
    int(&mut b, 0);
    let registry_slot = slot(&mut b);
    long(&mut b, 0);
    let names = [
        "/Script/Engine",
        "/Script/CoreUObject",
        "Class",
        "Package",
        if bp { "Blueprint" } else { "Texture2D" },
        "BlueprintGeneratedClass",
        "Actor",
        "EdGraph",
        "Function",
        "IntProperty",
        "中文测试",
        "中文测试_C",
        "EventGraph",
        "Tick",
        "MyValue",
        "Blueprint",
    ];
    patch(&mut b, name_count, names.len() as i32);
    let at = b.len();
    patch(&mut b, name_offset, at as i32);
    for s in names {
        wide(&mut b, s);
        int(&mut b, 0);
    }
    let at = b.len();
    patch(&mut b, import_offset_slot, at as i32);
    for (obj, class, outer, package) in [
        (0, 3, 0, 1),
        (1, 3, 0, 1),
        (4, 2, -1, 0),
        (5, 2, -1, 0),
        (6, 2, -1, 0),
        (7, 2, -1, 0),
        (8, 2, -2, 1),
        (9, 2, -2, 1),
    ] {
        name(&mut b, package);
        name(&mut b, class);
        int(&mut b, outer);
        name(&mut b, obj);
        name(&mut b, 0);
        if ue5 >= 1003 {
            int(&mut b, 0);
        }
    }
    let at = b.len();
    patch(&mut b, export_offset_slot, at as i32);
    let export_offset = at;
    for (class, super_index, outer, obj, asset) in [
        (-3, 0, 0, 10, true),
        (if bp { -4 } else { -3 }, -5, 0, 11, false),
        (-6, 0, 1, 12, false),
        (-7, 0, 2, 13, false),
        (-8, 0, 4, 14, false),
    ] {
        int(&mut b, class);
        int(&mut b, super_index);
        int(&mut b, 0);
        int(&mut b, outer);
        name(&mut b, obj);
        int(&mut b, 0);
        long(&mut b, 0);
        long(&mut b, 0);
        b.extend([0; 12]);
        if ue5 < 1005 {
            b.extend([0; 16]);
        }
        if ue5 >= 1006 {
            int(&mut b, 0);
        }
        int(&mut b, 0);
        int(&mut b, 0);
        int(&mut b, asset as i32);
        if ue5 >= 1003 {
            int(&mut b, 0);
        }
        b.extend([0; 20]);
        if ue5 >= 1010 {
            b.extend([0; 16]);
        }
    }
    let end = b.len();
    patch(&mut b, total, end as i32);
    Fixture {
        bytes: b,
        name_count,
        export_offset,
        registry_slot,
        name_offset,
    }
}

#[test]
fn unreal_preview_reads_versioned_utf16_names_imports_exports_and_graphs() -> Result<()> {
    for (ue5, marker) in [(0, None), (1013, Some(-265535)), (1017, None)] {
        let data = fixture(ue5, marker, true);
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("中文.uasset");
        fs::write(&path, &data.bytes)?;
        let result = inspect(&path, true).map_err(anyhow::Error::msg)?;
        assert!(result.is_blueprint);
        assert_eq!(
            property(&result.summary, "UE5 object version"),
            Some(ue5.to_string().as_str())
        );
        assert!(
            entries(&result.summary, "Name map")
                .iter()
                .any(|value| value.contains("中文测试"))
        );
        assert!(
            entries(&result.summary, "Graph objects")
                .iter()
                .any(|value| value.contains("中文测试.EventGraph"))
        );
        assert!(
            entries(&result.summary, "Function objects")
                .iter()
                .any(|value| value.contains("中文测试_C.Tick"))
        );
        assert!(
            entries(&result.summary, "Imported objects")
                .iter()
                .any(|value| value.contains("/Script/Engine"))
        );
        assert_eq!(
            property(&result.summary, "Generated class parents"),
            Some("/Script/Engine.Actor")
        );
        assert_eq!(fs::read(&path)?, data.bytes);
        assert_eq!(fs::read_dir(directory.path())?.count(), 1);
    }
    Ok(())
}

#[test]
fn blueprint_text_in_a_name_map_does_not_misclassify_a_texture() -> Result<()> {
    let data = fixture(1017, None, false);
    let result = inspect_reader(Cursor::new(data.bytes), true).map_err(anyhow::Error::msg)?;
    assert!(!result.is_blueprint);
    assert!(
        entries(&result.summary, "Name map")
            .iter()
            .any(|value| value.ends_with(" Blueprint"))
    );
    assert!(entries(&result.summary, "Graph objects").is_empty());
    Ok(())
}

#[test]
fn malformed_unreal_tables_keep_metadata_and_do_not_follow_huge_offsets() -> Result<()> {
    for mutate in 0..4 {
        let mut data = fixture(1017, None, true);
        match mutate {
            0 => patch(&mut data.bytes, data.name_count, i32::MAX),
            1 => patch(&mut data.bytes, data.name_offset, i32::MAX),
            2 => patch(&mut data.bytes, data.registry_slot, i32::MAX),
            _ => {
                data.bytes.truncate(data.export_offset + 112 * 3 + 10);
                let length = data.bytes.len();
                patch(&mut data.bytes, 44, length as i32);
            }
        }
        let result = inspect_reader(Cursor::new(data.bytes), true).map_err(anyhow::Error::msg)?;
        assert!(
            result.summary.note.is_some(),
            "Damage should remain visible for mutation {mutate}"
        );
        assert!(
            result
                .summary
                .sections
                .iter()
                .flat_map(|section| &section.items)
                .map(String::len)
                .sum::<usize>()
                <= super::super::binary::REPORT_LIMIT
        );
    }
    assert!(inspect_reader(Cursor::new(b"not an unreal package"), true).is_err());
    Ok(())
}
