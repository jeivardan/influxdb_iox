//! This module contains helper methods for constructing replicated writes
//! based on `DatabaseRules`.

use crate::schema::TIME_COLUMN_NAME;
use data_types::database_rules::Partitioner;
use generated_types::wal as wb;
use influxdb_line_protocol::{FieldValue, ParsedLine};

use std::{collections::BTreeMap, convert::TryFrom, fmt};

use chrono::Utc;
use crc32fast::Hasher;
use flatbuffers::FlatBufferBuilder;
use ouroboros::self_referencing;

pub fn type_description(value: wb::ColumnValue) -> &'static str {
    match value {
        wb::ColumnValue::TagValue => "tag",
        wb::ColumnValue::I64Value => "i64",
        wb::ColumnValue::U64Value => "u64",
        wb::ColumnValue::F64Value => "f64",
        wb::ColumnValue::BoolValue => "bool",
        wb::ColumnValue::StringValue => "String",
        wb::ColumnValue::NONE => "none",
        _ => "none",
    }
}

/// A friendlier wrapper to help deal with the Flatbuffers write data
#[self_referencing]
#[derive(Debug, Clone, PartialEq)]
pub struct ReplicatedWrite {
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    fb: wb::ReplicatedWrite<'this>,
    #[borrows(data)]
    #[covariant]
    write_buffer_batch: Option<wb::WriteBufferBatch<'this>>,
}

impl ReplicatedWrite {
    /// Returns the Flatbuffers struct for the WriteBufferBatch in the raw bytes
    /// of the payload of the ReplicatedWrite.
    pub fn write_buffer_batch(&self) -> Option<&wb::WriteBufferBatch<'_>> {
        self.borrow_write_buffer_batch().as_ref()
    }

    /// Returns the Flatbuffers struct for the ReplicatedWrite
    pub fn fb(&self) -> &wb::ReplicatedWrite<'_> {
        self.borrow_fb()
    }

    /// Returns true if this replicated write matches the writer and sequence.
    pub fn equal_to_writer_and_sequence(&self, writer_id: u32, sequence_number: u64) -> bool {
        self.fb().writer() == writer_id && self.fb().sequence() == sequence_number
    }

    /// Returns the writer id and sequence number
    pub fn writer_and_sequence(&self) -> (u32, u64) {
        (self.fb().writer(), self.fb().sequence())
    }

    /// Returns the serialized bytes for the write
    pub fn data(&self) -> &[u8] {
        self.borrow_data()
    }

    /// Returns the number of write buffer entries in this replicated write
    pub fn entry_count(&self) -> usize {
        self.write_buffer_batch()
            .map_or(0, |wbb| wbb.entries().map_or(0, |entries| entries.len()))
    }
}

impl TryFrom<Vec<u8>> for ReplicatedWrite {
    type Error = flatbuffers::InvalidFlatbuffer;

    fn try_from(data: Vec<u8>) -> Result<Self, Self::Error> {
        ReplicatedWriteTryBuilder {
            data,
            fb_builder: |data| flatbuffers::root::<wb::ReplicatedWrite<'_>>(data),
            write_buffer_batch_builder: |data| match flatbuffers::root::<wb::ReplicatedWrite<'_>>(
                data,
            )?
            .payload()
            {
                Some(payload) => Ok(Some(flatbuffers::root::<wb::WriteBufferBatch<'_>>(
                    &payload,
                )?)),
                None => Ok(None),
            },
        }
        .try_build()
    }
}

impl fmt::Display for ReplicatedWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fb = self.fb();
        write!(
            f,
            "\nwriter:{}, sequence:{}, checksum:{}\n",
            fb.writer(),
            fb.sequence(),
            fb.checksum()
        )?;

        if let Some(batch) = self.write_buffer_batch() {
            if let Some(entries) = batch.entries() {
                for entry in entries {
                    writeln!(f, "partition_key:{}", entry.partition_key().unwrap_or(""))?;

                    if let Some(tables) = entry.table_batches() {
                        for table in tables {
                            writeln!(f, "  table:{}", table.name().unwrap_or(""))?;

                            if let Some(rows) = table.rows() {
                                for row in rows {
                                    write!(f, "   ")?;
                                    if let Some(values) = row.values() {
                                        for value in values {
                                            let val = match value.value_type() {
                                                wb::ColumnValue::TagValue => value
                                                    .value_as_tag_value()
                                                    .unwrap()
                                                    .value()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                wb::ColumnValue::F64Value => value
                                                    .value_as_f64value()
                                                    .unwrap()
                                                    .value()
                                                    .to_string(),
                                                wb::ColumnValue::I64Value => value
                                                    .value_as_i64value()
                                                    .unwrap()
                                                    .value()
                                                    .to_string(),
                                                wb::ColumnValue::U64Value => value
                                                    .value_as_u64value()
                                                    .unwrap()
                                                    .value()
                                                    .to_string(),
                                                wb::ColumnValue::BoolValue => value
                                                    .value_as_bool_value()
                                                    .unwrap()
                                                    .value()
                                                    .to_string(),
                                                wb::ColumnValue::StringValue => value
                                                    .value_as_string_value()
                                                    .unwrap()
                                                    .value()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                wb::ColumnValue::NONE => "".to_string(),
                                                _ => "".to_string(),
                                            };
                                            write!(f, " {}:{}", value.column().unwrap_or(""), val)?;
                                        }
                                        writeln!(f)?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

pub fn lines_to_replicated_write(
    writer: u32,
    sequence: u64,
    lines: &[ParsedLine<'_>],
    partitioner: &impl Partitioner,
) -> ReplicatedWrite {
    let default_time = Utc::now();
    let entry_bytes = split_lines_into_write_entry_partitions(
        |line| partitioner.partition_key(line, &default_time).unwrap(),
        lines,
    );

    let mut hasher = Hasher::new();
    hasher.update(&entry_bytes);
    let checksum = hasher.finalize();

    let mut fbb = flatbuffers::FlatBufferBuilder::new_with_capacity(1024);
    let payload = fbb.create_vector_direct(&entry_bytes);

    let write = wb::ReplicatedWrite::create(
        &mut fbb,
        &wb::ReplicatedWriteArgs {
            writer,
            sequence,
            checksum,
            payload: Some(payload),
        },
    );

    fbb.finish(write, None);

    let (mut data, idx) = fbb.collapse();
    ReplicatedWrite::try_from(data.split_off(idx))
        .expect("Flatbuffer data just constructed should be valid")
}

pub fn split_lines_into_write_entry_partitions(
    partition_key_fn: impl Fn(&ParsedLine<'_>) -> String,
    lines: &[ParsedLine<'_>],
) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::new_with_capacity(1024);

    // split the lines into collections that go into partitions
    let mut partition_writes = BTreeMap::new();

    for line in lines {
        let key = partition_key_fn(line);

        partition_writes
            .entry(key)
            .or_insert_with(Vec::new)
            .push(line);
    }

    // create a WALEntry for each batch of lines going to a partition (one WALEntry
    // per partition)
    let entries = partition_writes
        .into_iter()
        .map(|(key, lines)| add_write_entry(&mut fbb, Some(&key), &lines))
        .collect::<Vec<_>>();

    let entries_vec = fbb.create_vector(&entries);

    let batch = wb::WriteBufferBatch::create(
        &mut fbb,
        &wb::WriteBufferBatchArgs {
            entries: Some(entries_vec),
        },
    );

    fbb.finish(batch, None);

    let (mut data, idx) = fbb.collapse();
    data.split_off(idx)
}

fn add_write_entry<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    partition_key: Option<&str>,
    lines: &[&ParsedLine<'_>],
) -> flatbuffers::WIPOffset<wb::WriteBufferEntry<'a>> {
    // split into tables
    let mut table_batches = BTreeMap::new();
    for line in lines {
        let measurement = line.series.measurement.as_str();
        table_batches
            .entry(measurement)
            .or_insert_with(Vec::new)
            .push(*line);
    }

    // create TableWriteBatch for each table
    let table_batches = table_batches
        .into_iter()
        .map(|(name, lines)| add_table_batch(fbb, name, &lines))
        .collect::<Vec<_>>();

    // create write entry
    let batches_vec = fbb.create_vector(&table_batches);

    let args = match partition_key {
        Some(key) => {
            let key = fbb.create_string(key);
            wb::WriteBufferEntryArgs {
                partition_key: Some(key),
                table_batches: Some(batches_vec),
                ..Default::default()
            }
        }
        None => wb::WriteBufferEntryArgs {
            table_batches: Some(batches_vec),
            ..Default::default()
        },
    };

    wb::WriteBufferEntry::create(fbb, &args)
}

fn add_table_batch<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    name: &str,
    lines: &[&ParsedLine<'_>],
) -> flatbuffers::WIPOffset<wb::TableWriteBatch<'a>> {
    // create Row
    let rows = lines
        .iter()
        .map(|line| add_line(fbb, line))
        .collect::<Vec<_>>();

    let table_name = fbb.create_string(name);
    let rows = fbb.create_vector(&rows);

    wb::TableWriteBatch::create(
        fbb,
        &wb::TableWriteBatchArgs {
            name: Some(table_name),
            rows: Some(rows),
        },
    )
}

fn add_line<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    line: &ParsedLine<'_>,
) -> flatbuffers::WIPOffset<wb::Row<'a>> {
    let mut row_values = Vec::new();

    if let Some(tags) = &line.series.tag_set {
        for (column, value) in tags {
            row_values.push(add_tag_value(fbb, column.as_str(), value.as_str()));
        }
    }

    for (column, value) in &line.field_set {
        let val = match value {
            FieldValue::I64(v) => add_i64_value(fbb, column.as_str(), *v),
            FieldValue::U64(v) => add_u64_value(fbb, column.as_str(), *v),
            FieldValue::F64(v) => add_f64_value(fbb, column.as_str(), *v),
            FieldValue::Boolean(v) => add_bool_value(fbb, column.as_str(), *v),
            FieldValue::String(v) => add_string_value(fbb, column.as_str(), v.as_str()),
        };

        row_values.push(val);
    }

    let time = line
        .timestamp
        .unwrap_or_else(|| Utc::now().timestamp_nanos());
    row_values.push(add_i64_value(fbb, TIME_COLUMN_NAME, time));

    let row_values = fbb.create_vector(&row_values);

    wb::Row::create(
        fbb,
        &wb::RowArgs {
            values: Some(row_values),
        },
    )
}

fn add_tag_value<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    column: &str,
    value: &str,
) -> flatbuffers::WIPOffset<wb::Value<'a>> {
    let value = fbb.create_string(&value);
    let tv = wb::TagValue::create(fbb, &wb::TagValueArgs { value: Some(value) });

    add_value(fbb, column, wb::ColumnValue::TagValue, tv.as_union_value())
}

fn add_string_value<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    column: &str,
    value: &str,
) -> flatbuffers::WIPOffset<wb::Value<'a>> {
    let value_offset = fbb.create_string(value);

    let sv = wb::StringValue::create(
        fbb,
        &wb::StringValueArgs {
            value: Some(value_offset),
        },
    );

    add_value(
        fbb,
        column,
        wb::ColumnValue::StringValue,
        sv.as_union_value(),
    )
}

fn add_f64_value<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    column: &str,
    value: f64,
) -> flatbuffers::WIPOffset<wb::Value<'a>> {
    let fv = wb::F64Value::create(fbb, &wb::F64ValueArgs { value });

    add_value(fbb, column, wb::ColumnValue::F64Value, fv.as_union_value())
}

fn add_i64_value<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    column: &str,
    value: i64,
) -> flatbuffers::WIPOffset<wb::Value<'a>> {
    let iv = wb::I64Value::create(fbb, &wb::I64ValueArgs { value });

    add_value(fbb, column, wb::ColumnValue::I64Value, iv.as_union_value())
}

fn add_u64_value<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    column: &str,
    value: u64,
) -> flatbuffers::WIPOffset<wb::Value<'a>> {
    let iv = wb::U64Value::create(fbb, &wb::U64ValueArgs { value });

    add_value(fbb, column, wb::ColumnValue::U64Value, iv.as_union_value())
}

fn add_bool_value<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    column: &str,
    value: bool,
) -> flatbuffers::WIPOffset<wb::Value<'a>> {
    let bv = wb::BoolValue::create(fbb, &wb::BoolValueArgs { value });

    add_value(fbb, column, wb::ColumnValue::BoolValue, bv.as_union_value())
}

fn add_value<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    column: &str,
    value_type: wb::ColumnValue,
    value: flatbuffers::WIPOffset<flatbuffers::UnionWIPOffset>,
) -> flatbuffers::WIPOffset<wb::Value<'a>> {
    let column = fbb.create_string(column);

    wb::Value::create(
        fbb,
        &wb::ValueArgs {
            column: Some(column),
            value_type,
            value: Some(value),
        },
    )
}
