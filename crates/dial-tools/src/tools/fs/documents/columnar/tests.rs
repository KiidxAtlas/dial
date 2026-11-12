use super::*;
use arrow_array::{Int64Array, StringArray};
use arrow_schema::{DataType, Field};
use std::sync::Arc;

fn batch() -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
    ]));
    let ids = Int64Array::from((0..250).collect::<Vec<i64>>());
    let names = StringArray::from(
        (0..250)
            .map(|i| (i % 7 != 3).then(|| format!("row-{i}")))
            .collect::<Vec<_>>(),
    );
    RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(names)]).unwrap()
}

fn parquet_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    let batch = batch();
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(&mut bytes, batch.schema(), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    bytes
}

#[test]
fn parquet_schema_and_pages_of_rows() {
    let bytes = parquet_bytes();
    let out = parquet(&bytes, None).unwrap();
    assert!(out.starts_with("Parquet: 250 rows; 1 row groups"), "{out}");
    assert!(
        out.contains("  id: Int64 (not null)\n  name: Utf8\n"),
        "{out}"
    );
    assert!(
        out.contains("[rows 0..100 of 250]\n#  | id | name\n---+----+-------\n0  | 0  | row-0\n"),
        "{out}"
    );
    assert!(out.contains("\n3  | 3  | NULL\n"), "{out}");
    assert!(out.contains("[more rows: :rows=100+100]"), "{out}");

    let tail = parquet(&bytes, Some("rows=240+50")).unwrap();
    assert!(tail.contains("[rows 240..250 of 250]\n"), "{tail}");
    assert!(tail.contains("\n240 | 240 | row-240\n"), "{tail}");
    assert!(!tail.contains("more rows"), "{tail}");
    let past = parquet(&bytes, Some("rows=300+5")).unwrap();
    assert!(
        past.contains("[rows 300..300 of 250]\n# | id | name\n"),
        "{past}"
    );
    assert!(parquet(&bytes, Some("rows=0+0")).is_err());
    assert!(parquet(&bytes, Some("sheet1")).is_err());
}

#[test]
fn arrow_ipc_files_and_streams_read_the_same_rows() {
    let batch = batch();
    let mut file = Vec::new();
    {
        let mut writer =
            arrow_ipc::writer::FileWriter::try_new(&mut file, &batch.schema()).unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
    }
    let mut stream = Vec::new();
    {
        let mut writer =
            arrow_ipc::writer::StreamWriter::try_new(&mut stream, &batch.schema()).unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
    }
    let from_file = arrow(&file, Some("rows=11+2")).unwrap();
    assert!(
        from_file.starts_with("Arrow IPC file: 250 rows; 1 record batches"),
        "{from_file}"
    );
    assert!(
        from_file.contains(
            "[rows 11..13 of 250]\n#  | id | name\n---+----+-------\n11 | 11 | row-11\n12 | 12 |"
        ),
        "{from_file}"
    );
    let from_stream = arrow(&stream, Some("rows=11+2")).unwrap();
    assert!(
        from_stream.starts_with("Arrow IPC stream: 250 rows"),
        "{from_stream}"
    );
    assert_eq!(
        from_file.split_once("[rows").unwrap().1,
        from_stream.split_once("[rows").unwrap().1
    );
}

#[test]
fn corrupt_parquet_is_refused() {
    let mut bytes = parquet_bytes();
    let len = bytes.len();
    bytes.truncate(len - 12);
    assert!(parquet(&bytes, None).is_err());
}
