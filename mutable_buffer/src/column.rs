use snafu::Snafu;

use crate::dictionary::{Dictionary, DID};
use data_types::partition_metadata::StatValues;
use generated_types::entry::LogicalColumnType;
use internal_types::entry::TypedValuesIterator;

use std::mem;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Don't know how to insert a column of type {}", inserted_value_type))]
    UnknownColumnType { inserted_value_type: String },

    #[snafu(display(
        "Unable to insert {} type into a column of {}",
        inserted_value_type,
        existing_column_type
    ))]
    TypeMismatch {
        existing_column_type: String,
        inserted_value_type: String,
    },

    #[snafu(display("InternalError: Applying i64 range on a column with non-i64 type"))]
    InternalTypeMismatchForTimePredicate,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Stores the actual data for columns in a chunk along with summary
/// statistics
#[derive(Debug, Clone)]
pub enum Column {
    F64(Vec<Option<f64>>, StatValues<f64>),
    I64(Vec<Option<i64>>, StatValues<i64>),
    U64(Vec<Option<u64>>, StatValues<u64>),
    String(Vec<Option<String>>, StatValues<String>),
    Bool(Vec<Option<bool>>, StatValues<bool>),
    Tag(Vec<Option<DID>>, StatValues<String>),
}

impl Column {
    /// Initializes a new column from typed values, the column on a table write
    /// batch on an Entry. Will initialize the stats with the first
    /// non-null value and update with any other non-null values included.
    pub fn new_from_typed_values(
        dictionary: &mut Dictionary,
        row_count: usize,
        logical_type: LogicalColumnType,
        values: TypedValuesIterator<'_>,
    ) -> Self {
        match values {
            TypedValuesIterator::String(vals) => match logical_type {
                LogicalColumnType::Tag => {
                    let mut tag_values = vec![None; row_count];
                    let mut stats: Option<StatValues<String>> = None;

                    let mut added_tag_values: Vec<_> = vals
                        .map(|tag| {
                            tag.map(|tag| {
                                match stats.as_mut() {
                                    Some(s) => StatValues::update_string(s, tag),
                                    None => {
                                        stats = Some(StatValues::new(tag.to_string()));
                                    }
                                }

                                dictionary.lookup_value_or_insert(tag)
                            })
                        })
                        .collect();

                    tag_values.append(&mut added_tag_values);

                    Self::Tag(
                        tag_values,
                        stats.expect("can't insert tag column with no values"),
                    )
                }
                LogicalColumnType::Field => {
                    let mut values = vec![None; row_count];
                    let mut stats: Option<StatValues<String>> = None;

                    for value in vals {
                        match value {
                            Some(v) => {
                                match stats.as_mut() {
                                    Some(s) => StatValues::update_string(s, v),
                                    None => stats = Some(StatValues::new(v.to_string())),
                                }

                                values.push(Some(v.to_string()));
                            }
                            None => values.push(None),
                        }
                    }

                    Self::String(
                        values,
                        stats.expect("can't insert string column with no values"),
                    )
                }
                _ => panic!("unsupported!"),
            },
            TypedValuesIterator::I64(vals) => {
                let mut values = vec![None; row_count];
                let mut stats: Option<StatValues<i64>> = None;

                for v in vals {
                    if let Some(val) = v {
                        match stats.as_mut() {
                            Some(s) => s.update(val),
                            None => stats = Some(StatValues::new(val)),
                        }
                    }
                    values.push(v);
                }

                Self::I64(
                    values,
                    stats.expect("can't insert i64 column with no values"),
                )
            }
            TypedValuesIterator::F64(vals) => {
                let mut values = vec![None; row_count];
                let mut stats: Option<StatValues<f64>> = None;

                for v in vals {
                    if let Some(val) = v {
                        match stats.as_mut() {
                            Some(s) => s.update(val),
                            None => stats = Some(StatValues::new(val)),
                        }
                    }
                    values.push(v);
                }

                Self::F64(
                    values,
                    stats.expect("can't insert f64 column with no values"),
                )
            }
            TypedValuesIterator::U64(vals) => {
                let mut values = vec![None; row_count];
                let mut stats: Option<StatValues<u64>> = None;

                for v in vals {
                    if let Some(val) = v {
                        match stats.as_mut() {
                            Some(s) => s.update(val),
                            None => stats = Some(StatValues::new(val)),
                        }
                    }
                    values.push(v);
                }

                Self::U64(
                    values,
                    stats.expect("can't insert u64 column with no values"),
                )
            }
            TypedValuesIterator::Bool(vals) => {
                let mut values = vec![None; row_count];
                let mut stats: Option<StatValues<bool>> = None;

                for v in vals {
                    if let Some(val) = v {
                        match stats.as_mut() {
                            Some(s) => s.update(val),
                            None => stats = Some(StatValues::new(val)),
                        }
                    }
                    values.push(v);
                }

                Self::Bool(
                    values,
                    stats.expect("can't insert bool column with no values"),
                )
            }
        }
    }

    /// Pushes typed values, the column from a table write batch on an Entry.
    /// Updates statsistics for any non-null values.
    pub fn push_typed_values(
        &mut self,
        dictionary: &mut Dictionary,
        logical_type: LogicalColumnType,
        values: TypedValuesIterator<'_>,
    ) -> Result<()> {
        match (self, values) {
            (Self::Bool(col, stats), TypedValuesIterator::Bool(values)) => {
                for val in values {
                    if let Some(v) = val {
                        stats.update(v)
                    };
                    col.push(val);
                }
            }
            (Self::I64(col, stats), TypedValuesIterator::I64(values)) => {
                for val in values {
                    if let Some(v) = val {
                        stats.update(v)
                    };
                    col.push(val);
                }
            }
            (Self::F64(col, stats), TypedValuesIterator::F64(values)) => {
                for val in values {
                    if let Some(v) = val {
                        stats.update(v)
                    };
                    col.push(val);
                }
            }
            (Self::U64(col, stats), TypedValuesIterator::U64(values)) => {
                for val in values {
                    if let Some(v) = val {
                        stats.update(v)
                    };
                    col.push(val);
                }
            }
            (Self::String(col, stats), TypedValuesIterator::String(values)) => {
                if logical_type != LogicalColumnType::Field {
                    TypeMismatch {
                        existing_column_type: "String",
                        inserted_value_type: "tag",
                    }
                    .fail()?;
                }

                for val in values {
                    match val {
                        Some(v) => {
                            StatValues::update_string(stats, v);
                            col.push(Some(v.to_string()));
                        }
                        None => col.push(None),
                    }
                }
            }
            (Self::Tag(col, stats), TypedValuesIterator::String(values)) => {
                if logical_type != LogicalColumnType::Tag {
                    TypeMismatch {
                        existing_column_type: "tag",
                        inserted_value_type: "String",
                    }
                    .fail()?;
                }

                for val in values {
                    match val {
                        Some(v) => {
                            StatValues::update_string(stats, v);
                            let id = dictionary.lookup_value_or_insert(v);
                            col.push(Some(id));
                        }
                        None => col.push(None),
                    }
                }
            }
            (existing, values) => TypeMismatch {
                existing_column_type: existing.type_description(),
                inserted_value_type: values.type_description(),
            }
            .fail()?,
        }

        Ok(())
    }

    /// Pushes None values onto the column until its len is equal to that passed
    /// in
    pub fn push_nulls_to_len(&mut self, len: usize) {
        match self {
            Self::Tag(vals, _) => {
                if len > vals.len() {
                    vals.resize(len, None);
                }
            }
            Self::I64(vals, _) => {
                if len > vals.len() {
                    vals.resize(len, None);
                }
            }
            Self::F64(vals, _) => {
                if len > vals.len() {
                    vals.resize(len, None);
                }
            }
            Self::U64(vals, _) => {
                if len > vals.len() {
                    vals.resize(len, None);
                }
            }
            Self::Bool(vals, _) => {
                if len > vals.len() {
                    vals.resize(len, None);
                }
            }
            Self::String(vals, _) => {
                if len > vals.len() {
                    vals.resize(len, None);
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::F64(v, _) => v.len(),
            Self::I64(v, _) => v.len(),
            Self::U64(v, _) => v.len(),
            Self::String(v, _) => v.len(),
            Self::Bool(v, _) => v.len(),
            Self::Tag(v, _) => v.len(),
        }
    }

    pub fn type_description(&self) -> &'static str {
        match self {
            Self::F64(_, _) => "f64",
            Self::I64(_, _) => "i64",
            Self::U64(_, _) => "u64",
            Self::String(_, _) => "String",
            Self::Bool(_, _) => "bool",
            Self::Tag(_, _) => "tag",
        }
    }

    pub fn get_i64_stats(&self) -> Option<StatValues<i64>> {
        match self {
            Self::I64(_, values) => Some(values.clone()),
            _ => None,
        }
    }

    /// The approximate memory size of the data in the column. Note that
    /// the space taken for the tag string values is represented in
    /// the dictionary size in the chunk that holds the table that has this
    /// column. The size returned here is only for their identifiers.
    pub fn size(&self) -> usize {
        match self {
            Self::F64(v, stats) => {
                mem::size_of::<Option<f64>>() * v.len() + mem::size_of_val(&stats)
            }
            Self::I64(v, stats) => {
                mem::size_of::<Option<i64>>() * v.len() + mem::size_of_val(&stats)
            }
            Self::U64(v, stats) => {
                mem::size_of::<Option<u64>>() * v.len() + mem::size_of_val(&stats)
            }
            Self::Bool(v, stats) => {
                mem::size_of::<Option<bool>>() * v.len() + mem::size_of_val(&stats)
            }
            Self::Tag(v, stats) => {
                mem::size_of::<Option<DID>>() * v.len() + mem::size_of_val(&stats)
            }
            Self::String(v, stats) => {
                let string_bytes_size = v
                    .iter()
                    .fold(0, |acc, val| acc + val.as_ref().map_or(0, |s| s.len()));
                let vec_pointer_sizes = mem::size_of::<Option<String>>() * v.len();
                string_bytes_size + vec_pointer_sizes + mem::size_of_val(&stats)
            }
        }
    }
}
