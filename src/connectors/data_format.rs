// Copyright © 2024 Pathway

use std::any::type_name;
use std::borrow::Cow;
use std::clone::Clone;
use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write;
use std::iter::zip;
use std::mem::take;
use std::str::{from_utf8, Utf8Error};

use crate::connectors::metadata::SourceMetadata;
use crate::connectors::ReaderContext::{Diff, Empty, KeyValue, RawBytes, TokenizedEntries};
use crate::connectors::{DataEventType, Offset, ReaderContext, SessionType, SnapshotEvent};
use crate::engine::error::{limit_length, DynError, DynResult, STANDARD_OBJECT_LENGTH_LIMIT};
use crate::engine::{Error, Key, Result, Timestamp, Type, Value};

use itertools::{chain, Itertools};
use log::error;
use serde::ser::{SerializeMap, Serializer};
use serde_json::json;
use serde_json::Value as JsonValue;

use super::data_storage::{ConversionError, SpecialEvent};

pub const COMMIT_LITERAL: &str = "*COMMIT*";
const DEBEZIUM_EMPTY_KEY_PAYLOAD: &str = "{\"payload\": {\"before\": {}, \"after\": {}}}";

fn is_commit_literal(value: &DynResult<Value>) -> bool {
    match value {
        Ok(Value::String(arc_str)) => arc_str.as_str() == COMMIT_LITERAL,
        Ok(Value::Bytes(arc_bytes)) => **arc_bytes == *COMMIT_LITERAL.as_bytes(),
        _ => false,
    }
}

type KeyFieldsWithErrors = Option<DynResult<Vec<Value>>>;
type ValueFieldsWithErrors = Vec<DynResult<Value>>;
pub type ErrorRemovalLogic = Box<dyn Fn(ValueFieldsWithErrors) -> DynResult<Vec<Value>>>;

#[derive(Debug)]
pub enum ParsedEventWithErrors {
    AdvanceTime,
    Insert((KeyFieldsWithErrors, ValueFieldsWithErrors)),

    // None as Vec of values means that the record is removed
    Upsert((KeyFieldsWithErrors, Option<ValueFieldsWithErrors>)),

    // If None, finding the key for the provided values becomes responsibility of the connector
    Delete((KeyFieldsWithErrors, ValueFieldsWithErrors)),
}

impl ParsedEventWithErrors {
    pub fn new(
        session_type: SessionType,
        data_event_type: DataEventType,
        key: KeyFieldsWithErrors,
        values: ValueFieldsWithErrors,
    ) -> Self {
        match session_type {
            SessionType::Native => {
                match data_event_type {
                    DataEventType::Insert => ParsedEventWithErrors::Insert((key, values)),
                    DataEventType::Delete => ParsedEventWithErrors::Delete((key, values)),
                    DataEventType::Upsert => panic!("incorrect Reader-Parser configuration: unexpected Upsert event in Native session"),
                }
            }
            SessionType::Upsert => {
                match data_event_type {
                    DataEventType::Insert => panic!("incorrect Reader-Parser configuration: unexpected Insert event in Upsert session"),
                    DataEventType::Delete => ParsedEventWithErrors::Upsert((key, None)),
                    DataEventType::Upsert => ParsedEventWithErrors::Upsert((key, Some(values))),
                }
            }
        }
    }
    pub fn remove_errors(self, logic: &ErrorRemovalLogic) -> DynResult<ParsedEvent> {
        match self {
            Self::AdvanceTime => Ok(ParsedEvent::AdvanceTime),
            Self::Insert((key, values)) => key
                .transpose()
                .and_then(|key| Ok(ParsedEvent::Insert((key, logic(values)?)))),
            Self::Upsert((key, values)) => key
                .transpose()
                .and_then(|key| Ok(ParsedEvent::Upsert((key, values.map(logic).transpose()?)))),
            Self::Delete((key, values)) => key
                .transpose()
                .and_then(|key| Ok(ParsedEvent::Delete((key, logic(values)?)))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParsedEvent {
    AdvanceTime,
    Insert((Option<Vec<Value>>, Vec<Value>)),

    // None as Vec of values means that the record is removed
    Upsert((Option<Vec<Value>>, Option<Vec<Value>>)),

    // If None, finding the key for the provided values becomes responsibility of the connector
    Delete((Option<Vec<Value>>, Vec<Value>)),
}

impl ParsedEvent {
    pub fn key(
        &self,
        mut values_to_key: impl FnMut(Option<&Vec<Value>>, Option<&Offset>) -> Key,
        offset: Option<&Offset>,
    ) -> Option<Key> {
        match self {
            ParsedEvent::Insert((raw_key, _))
            | ParsedEvent::Upsert((raw_key, _))
            | ParsedEvent::Delete((raw_key, _)) => Some(values_to_key(raw_key.as_ref(), offset)),
            ParsedEvent::AdvanceTime => None,
        }
    }

    pub fn snapshot_event(&self, key: Key) -> Option<SnapshotEvent> {
        match self {
            ParsedEvent::Insert((_, values)) => Some(SnapshotEvent::Insert(key, values.clone())),
            ParsedEvent::Upsert((_, values)) => Some(SnapshotEvent::Upsert(key, values.clone())),
            ParsedEvent::Delete((_, values)) => Some(SnapshotEvent::Delete(key, values.clone())),
            ParsedEvent::AdvanceTime => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    #[error("some fields weren't found in the header (fields present in table: {parsed:?}, fields specified in connector: {requested:?})")]
    FieldsNotFoundInHeader {
        parsed: Vec<String>,
        requested: Vec<String>,
    },

    #[error("failed to parse value {value:?} at field {field_name:?} according to the type {type_} in schema: {error}")]
    SchemaNotSatisfied {
        value: String,
        field_name: String,
        type_: Type,
        error: DynError,
    },

    #[error("too small number of csv tokens in the line: {0}")]
    UnexpectedNumberOfCsvTokens(usize),

    #[error("failed to create a field {field_name:?} with type {type_} from json payload: {}", limit_length(format!("{payload}"), STANDARD_OBJECT_LENGTH_LIMIT))]
    FailedToParseFromJson {
        field_name: String,
        payload: JsonValue,
        type_: Type,
    },

    #[error("key-value pair has unexpected number of tokens: {0} instead of 2")]
    KeyValueTokensIncorrect(usize),

    #[error("field {field_name} with {} is absent in {payload}", path.clone().map_or("no JsonPointer path specified".to_string(), |path| format!("path {path}")  ))]
    FailedToExtractJsonField {
        field_name: String,
        path: Option<String>,
        payload: JsonValue,
    },

    #[error("received message is not json: {0:?}")]
    FailedToParseJson(String),

    #[error("received metadata payload is not a valid json")]
    FailedToParseMetadata,

    #[error("received message doesn't comply with debezium format: {0}")]
    DebeziumFormatViolated(DebeziumFormatError),

    #[error("unknown debezium operation {0:?}")]
    UnsupportedDebeziumOperation(String),

    #[error("received message doesn't have payload")]
    EmptyKafkaPayload,

    #[error("internal error, reader context is not supported in this parser")]
    UnsupportedReaderContext,

    #[error("received plaintext message is not in utf-8 format: {0}")]
    Utf8DecodeFailed(#[from] Utf8Error),

    #[error("parsing {0} from an external datasource is not supported")]
    UnparsableType(Type),

    #[error("error in primary key, skipping the row: {0}")]
    ErrorInKey(DynError),

    #[error("no value for {field_name:?} field and no default specified")]
    NoDefault { field_name: String },
}

#[derive(Debug, thiserror::Error)]
pub enum DebeziumFormatError {
    #[error("incorrect type of payload.op field or it is missing")]
    OperationFieldMissing,

    #[error("there is no payload at the top level of value json")]
    NoPayloadAtTopLevel,

    #[error("the root of the JSON value is neither null nor map")]
    IncorrectJsonRoot,
}

impl From<DynError> for ParseError {
    fn from(value: DynError) -> Self {
        *value
            .downcast::<Self>()
            .expect("error should be ParseError")
    }
}

pub type ParseResult = DynResult<Vec<ParsedEventWithErrors>>;
type PrepareStringResult = Result<String, ParseError>;

#[derive(Clone, Debug)]
pub struct InnerSchemaField {
    type_: Type,
    default: Option<Value>, // None means that there is no default for the field
}

impl InnerSchemaField {
    pub fn new(type_: Type, default: Option<Value>) -> Self {
        Self { type_, default }
    }

    pub fn maybe_use_default(
        &self,
        name: &str,
        value: Option<Result<Value, Box<ConversionError>>>,
    ) -> DynResult<Value> {
        match value {
            Some(value) => Ok(value?),
            None => self.default.clone().ok_or(
                ParseError::NoDefault {
                    field_name: name.to_string(),
                }
                .into(),
            ),
        }
    }
}

fn prepare_plaintext_string(bytes: &[u8]) -> PrepareStringResult {
    Ok(from_utf8(bytes)?.trim().to_string())
}

pub trait Parser: Send {
    fn parse(&mut self, data: &ReaderContext) -> ParseResult;
    fn on_new_source_started(&mut self, metadata: Option<&SourceMetadata>);
    fn column_count(&self) -> usize;

    fn short_description(&self) -> Cow<'static, str> {
        type_name::<Self>().into()
    }

    fn session_type(&self) -> SessionType {
        SessionType::Native
    }
}

#[derive(Debug)]
pub struct FormatterContext {
    pub payloads: Vec<Vec<u8>>,
    pub key: Key,
    pub values: Vec<Value>,
    pub time: Timestamp,
    pub diff: isize,
}

impl FormatterContext {
    pub fn new(
        payloads: Vec<Vec<u8>>,
        key: Key,
        values: Vec<Value>,
        time: Timestamp,
        diff: isize,
    ) -> FormatterContext {
        FormatterContext {
            payloads,
            key,
            values,
            time,
            diff,
        }
    }

    pub fn new_single_payload(
        payload: Vec<u8>,
        key: Key,
        values: Vec<Value>,
        time: Timestamp,
        diff: isize,
    ) -> FormatterContext {
        FormatterContext {
            payloads: vec![payload],
            key,
            values,
            time,
            diff,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FormatterError {
    #[error("count of value columns and count of values mismatch")]
    ColumnsValuesCountMismatch,

    #[error("incorrect column index")]
    IncorrectColumnIndex,

    #[error("type {type_:?} is not json-serializable")]
    TypeNonJsonSerializable { type_: Type },

    #[error("Error value is not json-serializable")]
    ErrorValueNonJsonSerializable,

    #[error("this connector doesn't support this value type")]
    UnsupportedValueType,
}

pub trait Formatter: Send {
    fn format(
        &mut self,
        key: &Key,
        values: &[Value],
        time: Timestamp,
        diff: isize,
    ) -> Result<FormatterContext, FormatterError>;

    fn short_description(&self) -> Cow<'static, str> {
        type_name::<Self>().into()
    }
}

pub struct DsvSettings {
    key_column_names: Option<Vec<String>>,
    value_column_names: Vec<String>,
    separator: char,
}

impl DsvSettings {
    pub fn new(
        key_column_names: Option<Vec<String>>,
        value_column_names: Vec<String>,
        separator: char,
    ) -> DsvSettings {
        DsvSettings {
            key_column_names,
            value_column_names,
            separator,
        }
    }

    pub fn formatter(self) -> Box<dyn Formatter> {
        Box::new(DsvFormatter::new(self))
    }

    pub fn parser(self, schema: HashMap<String, InnerSchemaField>) -> Result<Box<dyn Parser>> {
        Ok(Box::new(DsvParser::new(self, schema)?))
    }
}

#[derive(Clone)]
enum DsvColumnIndex {
    IndexWithSchema(usize, InnerSchemaField),
    Metadata,
}

pub struct DsvParser {
    settings: DsvSettings,
    schema: HashMap<String, InnerSchemaField>,
    header: Vec<String>,

    metadata_column_value: Value,
    key_column_indices: Option<Vec<DsvColumnIndex>>,
    value_column_indices: Vec<DsvColumnIndex>,
    dsv_header_read: bool,
}

// We don't use `ParseBoolError` because its message only mentions "true" and "false"
// as possible representations. It can be misleading for the user since now we support
// more ways to represent a boolean value in the string.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
enum AdvancedBoolParseError {
    #[error("provided string was not parsable as a boolean value")]
    StringNotParsable,
}

/// Use modern Postgres true/false value names list
/// to parse boolean value from string
/// Related doc: `https://www.postgresql.org/docs/16/datatype-boolean.html`
///
/// We also support "t", "f", "y", "n" as single-letter prefixes
fn parse_bool_advanced(raw_value: &str) -> Result<bool, AdvancedBoolParseError> {
    let raw_value_lowercase = raw_value.trim().to_ascii_lowercase();
    match raw_value_lowercase.as_str() {
        "true" | "yes" | "on" | "1" | "t" | "y" => Ok(true),
        "false" | "no" | "off" | "0" | "f" | "n" => Ok(false),
        _ => Err(AdvancedBoolParseError::StringNotParsable),
    }
}

fn parse_with_type(
    raw_value: &str,
    schema: &InnerSchemaField,
    field_name: &str,
) -> DynResult<Value> {
    if let Some(default) = &schema.default {
        if raw_value.is_empty() && !matches!(schema.type_.unoptionalize(), Type::Any | Type::String)
        {
            return Ok(default.clone());
        }
    }

    match schema.type_.unoptionalize() {
        Type::Any | Type::String => Ok(Value::from(raw_value)),
        Type::Bool => Ok(Value::Bool(parse_bool_advanced(raw_value).map_err(
            |e| ParseError::SchemaNotSatisfied {
                field_name: field_name.to_string(),
                value: raw_value.to_string(),
                type_: schema.type_.clone(),
                error: Box::new(e),
            },
        )?)),
        Type::Int => Ok(Value::Int(raw_value.parse().map_err(|e| {
            ParseError::SchemaNotSatisfied {
                field_name: field_name.to_string(),
                value: raw_value.to_string(),
                type_: schema.type_.clone(),
                error: Box::new(e),
            }
        })?)),
        Type::Float => Ok(Value::Float(raw_value.parse().map_err(|e| {
            ParseError::SchemaNotSatisfied {
                field_name: field_name.to_string(),
                value: raw_value.to_string(),
                type_: schema.type_.clone(),
                error: Box::new(e),
            }
        })?)),
        Type::Json => {
            let json: JsonValue =
                serde_json::from_str(raw_value).map_err(|e| ParseError::SchemaNotSatisfied {
                    field_name: field_name.to_string(),
                    value: raw_value.to_string(),
                    type_: schema.type_.clone(),
                    error: Box::new(e),
                })?;
            Ok(Value::from(json))
        }
        _ => Err(ParseError::UnparsableType(schema.type_.clone()).into()),
    }
}

fn ensure_all_fields_in_schema(
    key_column_names: &Option<Vec<String>>,
    value_column_names: &Vec<String>,
    schema: &HashMap<String, InnerSchemaField>,
) -> Result<()> {
    for name in chain!(key_column_names.iter().flatten(), value_column_names) {
        if !schema.contains_key(name) {
            return Err(Error::FieldNotInSchema {
                name: name.clone(),
                schema_keys: schema.keys().cloned().collect(),
            });
        }
    }
    Ok(())
}

/// "magic field" containing the metadata
const METADATA_FIELD_NAME: &str = "_metadata";

impl DsvParser {
    pub fn new(
        settings: DsvSettings,
        schema: HashMap<String, InnerSchemaField>,
    ) -> Result<DsvParser> {
        ensure_all_fields_in_schema(
            &settings.key_column_names,
            &settings.value_column_names,
            &schema,
        )?;
        Ok(DsvParser {
            settings,
            schema,
            metadata_column_value: Value::None,
            header: Vec::new(),
            key_column_indices: None,
            value_column_indices: Vec::new(),
            dsv_header_read: false,
        })
    }

    fn column_indices_by_names(
        tokenized_entries: &[String],
        sought_names: &[String],
        schema: &HashMap<String, InnerSchemaField>,
    ) -> Result<Vec<DsvColumnIndex>, ParseError> {
        let mut value_indices_found = 0;

        let mut column_indices = vec![DsvColumnIndex::Metadata; sought_names.len()];
        let mut requested_indices = HashMap::<String, Vec<usize>>::new();
        for (index, field) in sought_names.iter().enumerate() {
            if field == METADATA_FIELD_NAME {
                value_indices_found += 1;
                continue;
            }
            match requested_indices.get_mut(field) {
                Some(indices) => indices.push(index),
                None => {
                    requested_indices.insert(field.clone(), vec![index]);
                }
            };
        }

        for (index, value) in tokenized_entries.iter().enumerate() {
            if let Some(indices) = requested_indices.get(value) {
                let schema_item = &schema[value];
                for requested_index in indices {
                    column_indices[*requested_index] =
                        DsvColumnIndex::IndexWithSchema(index, schema_item.clone());
                    value_indices_found += 1;
                }
            }
        }

        if value_indices_found == sought_names.len() {
            Ok(column_indices)
        } else {
            Err(ParseError::FieldsNotFoundInHeader {
                parsed: tokenized_entries.to_vec(),
                requested: sought_names.to_vec(),
            })
        }
    }

    fn parse_dsv_header(&mut self, tokenized_entries: &[String]) -> Result<(), ParseError> {
        self.key_column_indices = match &self.settings.key_column_names {
            Some(names) => Some(Self::column_indices_by_names(
                tokenized_entries,
                names,
                &self.schema,
            )?),
            None => None,
        };
        self.value_column_indices = Self::column_indices_by_names(
            tokenized_entries,
            &self.settings.value_column_names,
            &self.schema,
        )?;

        self.header = tokenized_entries.to_vec();
        self.dsv_header_read = true;
        Ok(())
    }

    fn parse_bytes_simple(&mut self, event: DataEventType, raw_bytes: &[u8]) -> ParseResult {
        let line = prepare_plaintext_string(raw_bytes)?;

        if line.is_empty() {
            return Ok(Vec::new());
        }

        if line == COMMIT_LITERAL {
            return Ok(vec![ParsedEventWithErrors::AdvanceTime]);
        }

        let tokens: Vec<String> = line
            .split(self.settings.separator)
            .map(std::string::ToString::to_string)
            .collect();
        self.parse_tokenized_entries(event, &tokens)
    }

    fn values_by_indices(
        &self,
        tokens: &[String],
        indices: &[DsvColumnIndex],
        header: &[String],
    ) -> ValueFieldsWithErrors {
        let mut parsed_tokens = Vec::with_capacity(indices.len());
        for index in indices {
            let token = match index {
                DsvColumnIndex::IndexWithSchema(index, schema_item) => {
                    parse_with_type(&tokens[*index], schema_item, &header[*index])
                }
                DsvColumnIndex::Metadata => Ok(self.metadata_column_value.clone()),
            };
            parsed_tokens.push(token);
        }
        parsed_tokens
    }

    fn parse_tokenized_entries(&mut self, event: DataEventType, tokens: &[String]) -> ParseResult {
        if tokens.len() == 1 {
            let line = &tokens[0];
            if line == COMMIT_LITERAL {
                return Ok(vec![ParsedEventWithErrors::AdvanceTime]);
            }
        }

        if !self.dsv_header_read {
            self.parse_dsv_header(tokens)?;
            return Ok(Vec::new());
        }

        let mut line_has_enough_tokens = true;
        if let Some(indices) = &self.key_column_indices {
            for index in indices {
                if let DsvColumnIndex::IndexWithSchema(index, _) = index {
                    line_has_enough_tokens &= index < &tokens.len();
                }
            }
        }
        for index in &self.value_column_indices {
            if let DsvColumnIndex::IndexWithSchema(index, _) = index {
                line_has_enough_tokens &= index < &tokens.len();
            }
        }
        if line_has_enough_tokens {
            let key = match &self.key_column_indices {
                Some(indices) => Some(
                    self.values_by_indices(tokens, indices, &self.header)
                        .into_iter()
                        .collect(),
                ),
                None => None,
            };
            let parsed_tokens =
                self.values_by_indices(tokens, &self.value_column_indices, &self.header);
            let parsed_entry = match event {
                DataEventType::Insert => ParsedEventWithErrors::Insert((key, parsed_tokens)),
                DataEventType::Delete => ParsedEventWithErrors::Delete((key, parsed_tokens)),
                DataEventType::Upsert => unreachable!("readers can't send upserts to DsvParser"),
            };
            Ok(vec![parsed_entry])
        } else {
            Err(ParseError::UnexpectedNumberOfCsvTokens(tokens.len()).into())
        }
    }
}

impl Parser for DsvParser {
    fn parse(&mut self, data: &ReaderContext) -> ParseResult {
        match data {
            RawBytes(event, raw_bytes) => self.parse_bytes_simple(*event, raw_bytes),
            TokenizedEntries(event, tokenized_entries) => {
                self.parse_tokenized_entries(*event, tokenized_entries)
            }
            KeyValue((_key, value)) => match value {
                Some(bytes) => self.parse_bytes_simple(DataEventType::Insert, bytes), // In Kafka we only have additions now
                None => Err(ParseError::EmptyKafkaPayload.into()),
            },
            Diff(_) => Err(ParseError::UnsupportedReaderContext.into()),
            Empty => Ok(vec![]),
        }
    }

    fn on_new_source_started(&mut self, metadata: Option<&SourceMetadata>) {
        self.dsv_header_read = false;
        if let Some(metadata) = metadata {
            let metadata_serialized: JsonValue =
                serde_json::to_value(metadata).expect("internal serialization error");
            self.metadata_column_value = metadata_serialized.into();
        }
    }

    fn column_count(&self) -> usize {
        self.settings.value_column_names.len()
    }
}

fn value_from_bytes(bytes: &[u8], parse_utf8: bool) -> DynResult<Value> {
    if parse_utf8 {
        Ok(Value::String(prepare_plaintext_string(bytes)?.into()))
    } else {
        Ok(Value::Bytes(bytes.into()))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum KeyGenerationPolicy {
    AlwaysAutogenerate,
    PreferMessageKey,
}

impl KeyGenerationPolicy {
    fn generate(self, key: &Option<Vec<u8>>, parse_utf8: bool) -> Option<DynResult<Vec<Value>>> {
        match &self {
            Self::AlwaysAutogenerate => None,
            Self::PreferMessageKey => key
                .as_ref()
                .map(|bytes| value_from_bytes(bytes, parse_utf8).map(|k| vec![k])),
        }
    }
}

pub struct IdentityParser {
    value_fields: Vec<String>,
    parse_utf8: bool,
    metadata_column_value: Value,
    session_type: SessionType,
    key_generation_policy: KeyGenerationPolicy,
}

impl IdentityParser {
    pub fn new(
        value_fields: Vec<String>,
        parse_utf8: bool,
        key_generation_policy: KeyGenerationPolicy,
        session_type: SessionType,
    ) -> IdentityParser {
        Self {
            value_fields,
            parse_utf8,
            metadata_column_value: Value::None,
            key_generation_policy,
            session_type,
        }
    }
}

impl Parser for IdentityParser {
    fn parse(&mut self, data: &ReaderContext) -> ParseResult {
        let (event, key, value, metadata) = match data {
            RawBytes(event, raw_bytes) => (
                *event,
                None,
                value_from_bytes(raw_bytes, self.parse_utf8),
                Ok(None),
            ),
            KeyValue((key, value)) => match value {
                Some(bytes) => (
                    DataEventType::Insert,
                    self.key_generation_policy.generate(key, self.parse_utf8),
                    value_from_bytes(bytes, self.parse_utf8),
                    Ok(None),
                ),
                None => return Err(ParseError::EmptyKafkaPayload.into()),
            },
            Diff(_) | TokenizedEntries(_, _) => {
                return Err(ParseError::UnsupportedReaderContext.into())
            }
            Empty => return Ok(vec![]),
        };

        let is_commit = is_commit_literal(&value);

        let event = if is_commit {
            ParsedEventWithErrors::AdvanceTime
        } else {
            let mut values = Vec::new();
            let mut metadata = Some(metadata);
            let mut value = Some(value);
            for field in &self.value_fields {
                let to_insert = if field == METADATA_FIELD_NAME {
                    metadata
                        .take()
                        .expect("metadata column should be used exactly once in IdentityParser")
                        .map(|metadata| metadata.unwrap_or(self.metadata_column_value.clone()))
                } else {
                    value
                        .take()
                        .expect("value column should be used exactly once in IdentityParser")
                };
                values.push(to_insert);
            }
            match self.session_type {
                SessionType::Native => {
                    match event {
                        DataEventType::Insert => ParsedEventWithErrors::Insert((key, values)),
                        DataEventType::Delete => ParsedEventWithErrors::Delete((key, values)),
                        DataEventType::Upsert => {
                            panic!("incorrect Reader-Parser configuration: unexpected Upsert event in Native session")
                        }
                    }
                }
                SessionType::Upsert => {
                    match event {
                        DataEventType::Insert => panic!("incorrect Reader-Parser configuration: unexpected Insert event in Upsert session"),
                        DataEventType::Delete => ParsedEventWithErrors::Upsert((key, None)),
                        DataEventType::Upsert => ParsedEventWithErrors::Upsert((key, Some(values))),
                    }
                }
            }
        };

        Ok(vec![event])
    }

    fn on_new_source_started(&mut self, metadata: Option<&SourceMetadata>) {
        if let Some(metadata) = metadata {
            let metadata_serialized: JsonValue =
                serde_json::to_value(metadata).expect("internal serialization error");
            self.metadata_column_value = metadata_serialized.into();
        }
    }

    fn column_count(&self) -> usize {
        self.value_fields.len()
    }

    fn session_type(&self) -> SessionType {
        self.session_type
    }
}

pub struct DsvFormatter {
    settings: DsvSettings,

    dsv_header_written: bool,
}

impl DsvFormatter {
    pub fn new(settings: DsvSettings) -> DsvFormatter {
        DsvFormatter {
            settings,

            dsv_header_written: false,
        }
    }
}

impl Formatter for DsvFormatter {
    fn format(
        &mut self,
        key: &Key,
        values: &[Value],
        time: Timestamp,
        diff: isize,
    ) -> Result<FormatterContext, FormatterError> {
        if values.len() != self.settings.value_column_names.len() {
            return Err(FormatterError::ColumnsValuesCountMismatch);
        }

        let mut sep_buf = [0; 4];
        let sep = self.settings.separator.encode_utf8(&mut sep_buf);

        let mut payloads = Vec::with_capacity(2);
        if !self.dsv_header_written {
            let mut header = Vec::new();
            write!(
                &mut header,
                "{}",
                self.settings
                    .value_column_names
                    .iter()
                    .map(String::as_str)
                    .chain(["time", "diff"])
                    .format(sep)
            )
            .expect("writing to vector should not fail");
            payloads.push(header);

            self.dsv_header_written = true;
        }

        let mut line = Vec::new();
        write!(
            &mut line,
            "{}",
            values
                .iter()
                .map(|v| v as &dyn Display)
                .chain([&time, &diff] as [&dyn Display; 2])
                .format(sep)
        )
        .unwrap();
        payloads.push(line);

        Ok(FormatterContext::new(
            payloads,
            *key,
            Vec::new(),
            time,
            diff,
        ))
    }
}

pub struct SingleColumnFormatter {
    value_field_index: usize,
}

impl SingleColumnFormatter {
    pub fn new(value_field_index: usize) -> SingleColumnFormatter {
        SingleColumnFormatter { value_field_index }
    }
}

impl Formatter for SingleColumnFormatter {
    fn format(
        &mut self,
        key: &Key,
        values: &[Value],
        time: Timestamp,
        diff: isize,
    ) -> Result<FormatterContext, FormatterError> {
        let payload = match &values
            .get(self.value_field_index)
            .ok_or(FormatterError::IncorrectColumnIndex)?
        {
            Value::Bytes(bytes) => bytes.to_vec(),
            Value::String(string) => string.as_bytes().to_vec(),
            _ => return Err(FormatterError::UnsupportedValueType),
        };
        Ok(FormatterContext::new_single_payload(
            payload,
            *key,
            values.to_vec(),
            time,
            diff,
        ))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum DebeziumDBType {
    Postgres,
    MongoDB,
}

pub struct DebeziumMessageParser {
    key_field_names: Option<Vec<String>>,
    value_field_names: Vec<String>,
    separator: String, // how key-value pair is separated
    db_type: DebeziumDBType,
}

fn parse_list_from_json(values: &[JsonValue], dtype: &Type) -> Option<Value> {
    let mut list = Vec::with_capacity(values.len());
    for value in values {
        list.push(parse_value_from_json(value, dtype)?);
    }
    Some(Value::from(list))
}

fn parse_tuple_from_json(values: &[JsonValue], dtypes: &[Type]) -> Option<Value> {
    if values.len() != dtypes.len() {
        return None;
    }
    let mut tuple = Vec::with_capacity(values.len());
    for (value, dtype) in values.iter().zip_eq(dtypes.iter()) {
        tuple.push(parse_value_from_json(value, dtype)?);
    }
    Some(Value::from(tuple))
}

fn parse_value_from_json(value: &JsonValue, dtype: &Type) -> Option<Value> {
    match (dtype, value) {
        (Type::Json, value) => Some(Value::from(value.clone())),
        (Type::Optional(_) | Type::Any, JsonValue::Null) => Some(Value::None),
        (Type::Optional(arg), value) => parse_value_from_json(value, arg),
        (Type::String | Type::Any, JsonValue::String(s)) => Some(Value::from(s.as_str())),
        (Type::Int, JsonValue::Number(v)) => Some(Value::from(v.as_i64()?)),
        (Type::Float, JsonValue::Number(v)) => v.as_f64().map(Value::from),
        (Type::Any, JsonValue::Number(v)) => {
            if let Some(parsed_i64) = v.as_i64() {
                Some(Value::from(parsed_i64))
            } else {
                Some(Value::from(v.as_f64()?))
            }
        }
        (Type::Bool | Type::Any, JsonValue::Bool(v)) => Some(Value::Bool(*v)),
        (Type::Tuple(dtypes), JsonValue::Array(v)) => parse_tuple_from_json(v, dtypes),
        (Type::List(arg), JsonValue::Array(v)) => parse_list_from_json(v, arg),
        (Type::Any, JsonValue::Array(v)) => parse_list_from_json(v, &Type::Any),
        _ => None,
    }
}

fn serialize_value_to_json(value: &Value) -> Result<JsonValue, FormatterError> {
    match value {
        Value::None => Ok(JsonValue::Null),
        Value::Int(i) => Ok(json!(i)),
        Value::Float(f) => Ok(json!(f)),
        Value::Bool(b) => Ok(json!(b)),
        Value::String(s) => Ok(json!(s)),
        Value::Pointer(p) => Ok(json!(p.to_string())),
        Value::Tuple(t) => {
            let mut items = Vec::with_capacity(t.len());
            for item in t.iter() {
                items.push(serialize_value_to_json(item)?);
            }
            Ok(JsonValue::Array(items))
        }
        Value::Bytes(b) => {
            let mut items = Vec::with_capacity(b.len());
            for item in b.iter() {
                items.push(json!(item));
            }
            Ok(JsonValue::Array(items))
        }
        Value::IntArray(a) => {
            let mut items = Vec::with_capacity(a.len());
            for item in a.iter() {
                items.push(json!(item));
            }
            Ok(JsonValue::Array(items))
        }
        Value::FloatArray(a) => {
            let mut items = Vec::with_capacity(a.len());
            for item in a.iter() {
                items.push(json!(item));
            }
            Ok(JsonValue::Array(items))
        }
        Value::DateTimeNaive(dt) => Ok(json!(dt.to_string())),
        Value::DateTimeUtc(dt) => Ok(json!(dt.to_string())),
        Value::Duration(d) => Ok(json!(d.nanoseconds())),
        Value::Json(j) => Ok((**j).clone()),
        Value::Error => Err(FormatterError::ErrorValueNonJsonSerializable),
        Value::PyObjectWrapper(_) => Err(FormatterError::TypeNonJsonSerializable {
            type_: Type::PyObjectWrapper,
        }),
    }
}

fn values_by_names_from_json(
    payload: &JsonValue,
    field_names: &[String],
    column_paths: &HashMap<String, String>,
    field_absence_is_error: bool,
    schema: &HashMap<String, InnerSchemaField>,
    metadata_column_value: &Value,
) -> ValueFieldsWithErrors {
    let mut parsed_values = Vec::with_capacity(field_names.len());
    for value_field in field_names {
        let (default_value, dtype) = {
            if let Some(schema_item) = schema.get(value_field) {
                (schema_item.default.as_ref(), &schema_item.type_)
            } else {
                (None, &Type::Any)
            }
        };

        let value = if value_field == METADATA_FIELD_NAME {
            Ok(metadata_column_value.clone())
        } else if let Some(path) = column_paths.get(value_field) {
            if let Some(value) = payload.pointer(path) {
                parse_value_from_json(value, dtype).ok_or_else(|| {
                    ParseError::FailedToParseFromJson {
                        field_name: value_field.to_string(),
                        payload: value.clone(),
                        type_: dtype.clone(),
                    }
                    .into()
                })
            } else if let Some(default) = default_value {
                Ok(default.clone())
            } else if field_absence_is_error {
                Err(ParseError::FailedToExtractJsonField {
                    field_name: value_field.to_string(),
                    path: Some(path.to_string()),
                    payload: payload.clone(),
                }
                .into())
            } else {
                Ok(Value::None)
            }
        } else {
            let value_specified_in_json = payload.get(value_field).is_some();

            if value_specified_in_json {
                parse_value_from_json(&payload[&value_field], dtype).ok_or_else(|| {
                    ParseError::FailedToParseFromJson {
                        field_name: value_field.to_string(),
                        payload: payload[&value_field].clone(),
                        type_: dtype.clone(),
                    }
                    .into()
                })
            } else if let Some(default) = default_value {
                Ok(default.clone())
            } else if field_absence_is_error {
                Err(ParseError::FailedToExtractJsonField {
                    field_name: value_field.to_string(),
                    path: None,
                    payload: payload.clone(),
                }
                .into())
            } else {
                Ok(Value::None)
            }
        };
        parsed_values.push(value);
    }
    parsed_values
}

impl DebeziumMessageParser {
    pub fn new(
        key_field_names: Option<Vec<String>>,
        value_field_names: Vec<String>,
        separator: String,
        db_type: DebeziumDBType,
    ) -> DebeziumMessageParser {
        DebeziumMessageParser {
            key_field_names,
            value_field_names,
            separator,
            db_type,
        }
    }

    pub fn standard_separator() -> String {
        "        ".to_string()
    }

    fn parse_event(
        &mut self,
        key: &JsonValue,
        value: &JsonValue,
        event: DataEventType,
    ) -> Result<ParsedEventWithErrors, ParseError> {
        // in case of MongoDB, the message is always string
        let prepared_value: JsonValue = {
            if let JsonValue::String(serialized_json) = &value {
                let Ok(prepared_value) = serde_json::from_str::<JsonValue>(serialized_json) else {
                    return Err(ParseError::FailedToParseJson(serialized_json.to_string()));
                };
                prepared_value
            } else {
                value.clone()
            }
        };

        let key = self.key_field_names.as_ref().map(|names| {
            values_by_names_from_json(
                key,
                names,
                &HashMap::new(),
                true,
                &HashMap::new(),
                &Value::None,
            )
            .into_iter()
            .collect()
        });

        let parsed_values = values_by_names_from_json(
            &prepared_value,
            &self.value_field_names,
            &HashMap::new(),
            true,
            &HashMap::new(),
            &Value::None,
        );

        match event {
            DataEventType::Insert => Ok(ParsedEventWithErrors::Insert((key, parsed_values))),
            DataEventType::Delete => Ok(ParsedEventWithErrors::Delete((key, parsed_values))),
            DataEventType::Upsert => Ok(ParsedEventWithErrors::Upsert((key, Some(parsed_values)))),
        }
    }

    fn parse_read_or_create(&mut self, key: &JsonValue, value: &JsonValue) -> ParseResult {
        let event = match self.db_type {
            DebeziumDBType::Postgres => {
                self.parse_event(key, &value["after"], DataEventType::Insert)?
            }
            DebeziumDBType::MongoDB => {
                self.parse_event(key, &value["after"], DataEventType::Upsert)?
            }
        };
        Ok(vec![event])
    }

    fn parse_delete(&mut self, key: &JsonValue, value: &JsonValue) -> ParseResult {
        let event = match self.db_type {
            DebeziumDBType::Postgres => {
                self.parse_event(key, &value["before"], DataEventType::Delete)?
            }
            DebeziumDBType::MongoDB => {
                let key = self.key_field_names.as_ref().map(|names| {
                    values_by_names_from_json(
                        key,
                        names,
                        &HashMap::new(),
                        true,
                        &HashMap::new(),
                        &Value::None,
                    )
                    .into_iter()
                    .collect()
                });
                ParsedEventWithErrors::Upsert((key, None))
            }
        };
        Ok(vec![event])
    }

    fn parse_update(&mut self, key: &JsonValue, value: &JsonValue) -> ParseResult {
        match self.db_type {
            DebeziumDBType::Postgres => {
                let event_before =
                    self.parse_event(key, &value["before"], DataEventType::Delete)?;
                let event_after = self.parse_event(key, &value["after"], DataEventType::Insert)?;
                Ok(vec![event_before, event_after])
            }
            DebeziumDBType::MongoDB => {
                let event_after = self.parse_event(key, &value["after"], DataEventType::Upsert)?;
                Ok(vec![event_after])
            }
        }
    }
}

impl Parser for DebeziumMessageParser {
    fn parse(&mut self, data: &ReaderContext) -> ParseResult {
        let (raw_key_change, raw_value_change) = match data {
            RawBytes(event, raw_bytes) => {
                // We don't use `event` type here, because it's Debezium message parser,
                // whose messages can only arrive from Kafka.
                //
                // There is no snapshot scenario for Kafka.
                //
                // In addition, this branch of `match` condition is only used in unit-tests.
                assert_eq!(event, &DataEventType::Insert);

                let line = prepare_plaintext_string(raw_bytes)?;

                let key_and_value: Vec<&str> = line.trim().split(&self.separator).collect();
                if key_and_value.len() != 2 {
                    return Err(ParseError::KeyValueTokensIncorrect(key_and_value.len()).into());
                }
                (key_and_value[0].to_string(), key_and_value[1].to_string())
            }
            KeyValue((k, v)) => {
                let key = match k {
                    Some(bytes) => prepare_plaintext_string(bytes)?,
                    None => {
                        if self.key_field_names.is_some() {
                            return Err(ParseError::EmptyKafkaPayload.into());
                        }
                        DEBEZIUM_EMPTY_KEY_PAYLOAD.to_string()
                    }
                };
                let value = match v {
                    Some(bytes) => prepare_plaintext_string(bytes)?,
                    None => return Err(ParseError::EmptyKafkaPayload.into()),
                };
                (key, value)
            }
            Diff(_) | TokenizedEntries(_, _) | Empty => {
                return Err(ParseError::UnsupportedReaderContext.into());
            }
        };

        let Ok(value_change) = serde_json::from_str(&raw_value_change) else {
            return Err(ParseError::FailedToParseJson(raw_value_change).into());
        };

        let change_payload = match value_change {
            JsonValue::Object(payload_value) => payload_value,
            JsonValue::Null => return Ok(Vec::new()), // tombstone event for kafka: nothing to do for us
            _ => {
                return Err(ParseError::DebeziumFormatViolated(
                    DebeziumFormatError::IncorrectJsonRoot,
                )
                .into())
            }
        };

        let Ok(change_key) = serde_json::from_str::<JsonValue>(&raw_key_change) else {
            return Err(ParseError::FailedToParseJson(raw_key_change).into());
        };

        if !change_payload.contains_key("payload") {
            return Err(ParseError::DebeziumFormatViolated(
                DebeziumFormatError::NoPayloadAtTopLevel,
            )
            .into());
        }

        match &change_payload["payload"]["op"] {
            JsonValue::String(op) => match op.as_ref() {
                "r" | "c" => {
                    self.parse_read_or_create(&change_key["payload"], &change_payload["payload"])
                }
                "u" => self.parse_update(&change_key["payload"], &change_payload["payload"]),
                "d" => self.parse_delete(&change_key["payload"], &change_payload["payload"]),
                _ => Err(ParseError::UnsupportedDebeziumOperation(op.to_string()).into()),
            },
            _ => Err(ParseError::DebeziumFormatViolated(
                DebeziumFormatError::OperationFieldMissing,
            )
            .into()),
        }
    }

    fn on_new_source_started(&mut self, _metadata: Option<&SourceMetadata>) {}

    fn column_count(&self) -> usize {
        self.value_field_names.len()
    }

    fn session_type(&self) -> SessionType {
        match self.db_type {
            DebeziumDBType::Postgres => SessionType::Native,

            // MongoDB events don't contain the previous state of the record
            // therefore we can only do the upsert with the same key and the
            // new value
            DebeziumDBType::MongoDB => SessionType::Upsert,
        }
    }
}

pub struct JsonLinesParser {
    key_field_names: Option<Vec<String>>,
    value_field_names: Vec<String>,
    column_paths: HashMap<String, String>,
    field_absence_is_error: bool,
    schema: HashMap<String, InnerSchemaField>,
    metadata_column_value: Value,
    session_type: SessionType,
}

impl JsonLinesParser {
    pub fn new(
        key_field_names: Option<Vec<String>>,
        value_field_names: Vec<String>,
        column_paths: HashMap<String, String>,
        field_absence_is_error: bool,
        schema: HashMap<String, InnerSchemaField>,
        session_type: SessionType,
    ) -> Result<JsonLinesParser> {
        ensure_all_fields_in_schema(&key_field_names, &value_field_names, &schema)?;
        Ok(JsonLinesParser {
            key_field_names,
            value_field_names,
            column_paths,
            field_absence_is_error,
            schema,
            metadata_column_value: Value::None,
            session_type,
        })
    }
}

impl Parser for JsonLinesParser {
    fn parse(&mut self, data: &ReaderContext) -> ParseResult {
        let (data_event, key, line) = match data {
            RawBytes(event, line) => {
                let line = prepare_plaintext_string(line)?;
                (*event, None, line)
            }
            KeyValue((_key, value)) => {
                if let Some(line) = value {
                    let line = prepare_plaintext_string(line)?;
                    (DataEventType::Insert, None, line)
                } else {
                    return Err(ParseError::EmptyKafkaPayload.into());
                }
            }
            Diff(_) | TokenizedEntries(..) => {
                return Err(ParseError::UnsupportedReaderContext.into());
            }
            Empty => return Ok(vec![]),
        };

        if line.is_empty() {
            return Ok(vec![]);
        }

        if line == COMMIT_LITERAL {
            return Ok(vec![ParsedEventWithErrors::AdvanceTime]);
        }

        let payload: JsonValue = match serde_json::from_str(&line) {
            Ok(json_value) => json_value,
            Err(_) => return Err(ParseError::FailedToParseJson(line).into()),
        };

        let key = key.or(match &self.key_field_names {
            Some(key_field_names) => Some(
                values_by_names_from_json(
                    &payload,
                    key_field_names,
                    &self.column_paths,
                    self.field_absence_is_error,
                    &self.schema,
                    &self.metadata_column_value,
                )
                .into_iter()
                .collect(),
            ),
            None => None, // use method from the different PR
        });

        let values = values_by_names_from_json(
            &payload,
            &self.value_field_names,
            &self.column_paths,
            self.field_absence_is_error,
            &self.schema,
            &self.metadata_column_value,
        );

        let event = ParsedEventWithErrors::new(self.session_type, data_event, key, values);

        Ok(vec![event])
    }

    fn on_new_source_started(&mut self, metadata: Option<&SourceMetadata>) {
        if let Some(metadata) = metadata {
            let metadata_serialized: JsonValue =
                serde_json::to_value(metadata).expect("internal serialization error");
            self.metadata_column_value = metadata_serialized.into();
        }
    }

    fn column_count(&self) -> usize {
        self.value_field_names.len()
    }

    fn session_type(&self) -> SessionType {
        self.session_type
    }
}

/// Receives values directly from a Reader and passes them
/// further only making adjustments according to the schema.
///
/// It is useful when no raw values parsing is needed.
pub struct TransparentParser {
    key_field_names: Option<Vec<String>>,
    value_field_names: Vec<String>,
    schema: HashMap<String, InnerSchemaField>,
    session_type: SessionType,
}

impl TransparentParser {
    pub fn new(
        key_field_names: Option<Vec<String>>,
        value_field_names: Vec<String>,
        schema: HashMap<String, InnerSchemaField>,
        session_type: SessionType,
    ) -> Result<TransparentParser> {
        ensure_all_fields_in_schema(&key_field_names, &value_field_names, &schema)?;
        Ok(TransparentParser {
            key_field_names,
            value_field_names,
            schema,
            session_type,
        })
    }
}

impl Parser for TransparentParser {
    fn parse(&mut self, data: &ReaderContext) -> ParseResult {
        let (data_event, key, values) = match data {
            Empty => return Ok(vec![]),
            Diff((data_event, key, values)) => (data_event, key, values),
            _ => return Err(ParseError::UnsupportedReaderContext.into()),
        };
        if values.get_special() == Some(SpecialEvent::Commit) {
            return Ok(vec![ParsedEventWithErrors::AdvanceTime]);
        }
        let key = key.clone().map(Ok).or_else(|| {
            self.key_field_names.as_ref().map(|key_field_names| {
                key_field_names
                    .iter()
                    .map(|name| {
                        self.schema[name] // ensure_all_fields_in_schema in new() makes sure that all keys are in the schema
                        .maybe_use_default(name, values.get(name).cloned())
                    })
                    .collect()
            })
        });

        let values: Vec<_> = self
            .value_field_names
            .iter()
            .map(|name| {
                self.schema[name] // ensure_all_fields_in_schema in new() makes sure that all keys are in the schema
                    .maybe_use_default(name, values.get(name).cloned())
            })
            .collect();

        let event = ParsedEventWithErrors::new(self.session_type, *data_event, key, values);

        Ok(vec![event])
    }

    fn on_new_source_started(&mut self, _metadata: Option<&SourceMetadata>) {}

    fn column_count(&self) -> usize {
        self.value_field_names.len()
    }

    fn session_type(&self) -> SessionType {
        self.session_type
    }
}

#[derive(Debug)]
pub struct PsqlUpdatesFormatter {
    table_name: String,
    value_field_names: Vec<String>,
}

impl PsqlUpdatesFormatter {
    pub fn new(table_name: String, value_field_names: Vec<String>) -> PsqlUpdatesFormatter {
        PsqlUpdatesFormatter {
            table_name,
            value_field_names,
        }
    }
}

impl Formatter for PsqlUpdatesFormatter {
    fn format(
        &mut self,
        key: &Key,
        values: &[Value],
        time: Timestamp,
        diff: isize,
    ) -> Result<FormatterContext, FormatterError> {
        if values.len() != self.value_field_names.len() {
            return Err(FormatterError::ColumnsValuesCountMismatch);
        }

        let mut result = Vec::new();
        writeln!(
            result,
            "INSERT INTO {} ({},time,diff) VALUES ({},{},{})",
            self.table_name,
            self.value_field_names.iter().join(","),
            (1..=values.len()).format_with(",", |x, f| f(&format_args!("${x}"))),
            time,
            diff
        )
        .unwrap();

        Ok(FormatterContext::new_single_payload(
            result,
            *key,
            values.to_vec(),
            time,
            diff,
        ))
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PsqlSnapshotFormatterError {
    #[error("repeated value field {0:?}")]
    RepeatedValueField(String),

    #[error("unknown key field {0:?}")]
    UnknownKey(String),
}

#[derive(Debug)]
pub struct PsqlSnapshotFormatter {
    table_name: String,
    key_field_names: Vec<String>,
    value_field_names: Vec<String>,

    key_field_positions: Vec<usize>,
    value_field_positions: Vec<usize>,
    custom_expressions: HashMap<String, String>,
}

impl PsqlSnapshotFormatter {
    pub fn new(
        table_name: String,
        mut key_field_names: Vec<String>,
        mut value_field_names: Vec<String>,
        custom_expressions: Option<HashMap<String, String>>,
    ) -> Result<PsqlSnapshotFormatter, PsqlSnapshotFormatterError> {
        let mut field_positions = HashMap::<String, usize>::new();
        for (index, field_name) in value_field_names.iter_mut().enumerate() {
            if field_positions.contains_key(field_name) {
                return Err(PsqlSnapshotFormatterError::RepeatedValueField(take(
                    field_name,
                )));
            }
            field_positions.insert(field_name.clone(), index);
        }

        let mut key_field_positions = Vec::with_capacity(key_field_names.len());
        for key_field_name in &mut key_field_names {
            let position = field_positions
                .remove(key_field_name)
                .ok_or_else(|| PsqlSnapshotFormatterError::UnknownKey(take(key_field_name)))?;
            key_field_positions.push(position);
        }

        let mut value_field_positions: Vec<_> = field_positions.into_values().collect();

        key_field_positions.sort_unstable();
        value_field_positions.sort_unstable();
        Ok(PsqlSnapshotFormatter {
            table_name,
            key_field_names,
            value_field_names,

            key_field_positions,
            value_field_positions,
            custom_expressions: custom_expressions.unwrap_or_default(),
        })
    }
}

impl Formatter for PsqlSnapshotFormatter {
    fn format(
        &mut self,
        key: &Key,
        values: &[Value],
        time: Timestamp,
        diff: isize,
    ) -> Result<FormatterContext, FormatterError> {
        if values.len() != self.value_field_names.len() {
            return Err(FormatterError::ColumnsValuesCountMismatch);
        }

        let mut result = Vec::new();

        let update_condition = self
            .key_field_positions
            .iter()
            .map(|position| {
                format!(
                    "{}.{}=${}",
                    self.table_name,
                    self.value_field_names[*position],
                    *position + 1
                )
            })
            .join(" AND ");

        let update_pairs = self
            .value_field_names
            .iter()
            .map(|field_name| format!("{field_name}=excluded.{field_name}"))
            .join(",");

        let insert_values = (1..=values.len())
            .format_with(",", |x, f| {
                if let Some(custom_expression) = self.custom_expressions.get(&self.value_field_names[x - 1]) {
                    let substituted = custom_expression.replace("$?", &format!("${}", x));
                    f(&substituted)
                } else {
                    f(&format_args!("${x}"))
                }
            });

        writeln!(
            result,
            "INSERT INTO {table} ({insert_columns}, time, diff)\
            VALUES ({insert_values}, {time}, {diff})\
            ON CONFLICT DO UPDATE SET {on_conflict_update}, time={time}, diff={diff}
            WHERE {update_condition} AND ({table}.time<{time} OR ({table}.time={time} AND {table}.diff=-1))
            ",
            table=self.table_name,
            insert_columns=self.value_field_names.iter().join(","),
            insert_values=insert_values,
            time=time,
            diff=diff,
            on_conflict_update=update_pairs,
            update_condition=update_condition,
        ).unwrap();

        Ok(FormatterContext::new_single_payload(
            result,
            *key,
            values.to_vec(),
            time,
            diff,
        ))
    }
}

#[derive(Debug)]
pub struct JsonLinesFormatter {
    value_field_names: Vec<String>,
}

impl JsonLinesFormatter {
    pub fn new(value_field_names: Vec<String>) -> JsonLinesFormatter {
        JsonLinesFormatter { value_field_names }
    }
}

impl Formatter for JsonLinesFormatter {
    fn format(
        &mut self,
        key: &Key,
        values: &[Value],
        time: Timestamp,
        diff: isize,
    ) -> Result<FormatterContext, FormatterError> {
        let mut serializer = serde_json::Serializer::new(Vec::<u8>::new());
        let mut map = serializer
            .serialize_map(Some(self.value_field_names.len() + 2))
            .unwrap();
        for (key, value) in zip(self.value_field_names.iter(), values) {
            map.serialize_entry(key, &serialize_value_to_json(value)?)
                .unwrap();
        }
        map.serialize_entry("diff", &diff).unwrap();
        map.serialize_entry("time", &time).unwrap();
        map.end().unwrap();

        Ok(FormatterContext::new_single_payload(
            serializer.into_inner(),
            *key,
            Vec::new(),
            time,
            diff,
        ))
    }
}

pub struct NullFormatter {}

impl NullFormatter {
    pub fn new() -> NullFormatter {
        NullFormatter {}
    }
}

impl Default for NullFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl Formatter for NullFormatter {
    fn format(
        &mut self,
        key: &Key,
        _values: &[Value],
        time: Timestamp,
        diff: isize,
    ) -> Result<FormatterContext, FormatterError> {
        Ok(FormatterContext::new(
            Vec::new(),
            *key,
            Vec::new(),
            time,
            diff,
        ))
    }
}

#[derive(Default)]
pub struct IdentityFormatter {}

impl IdentityFormatter {
    pub fn new() -> IdentityFormatter {
        IdentityFormatter {}
    }
}

impl Formatter for IdentityFormatter {
    fn format(
        &mut self,
        key: &Key,
        values: &[Value],
        time: Timestamp,
        diff: isize,
    ) -> Result<FormatterContext, FormatterError> {
        Ok(FormatterContext::new_single_payload(
            Vec::new(),
            *key,
            values.to_vec(),
            time,
            diff,
        ))
    }
}
