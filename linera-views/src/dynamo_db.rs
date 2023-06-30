// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    batch::{Batch, DeletePrefixExpander, SimpleUnorderedBatch},
    common::{ContextFromDb, KeyIterable, KeyValueIterable, KeyValueStoreClient, MIN_VIEW_TAG},
    localstack,
    lru_caching::LruCachingKeyValueClient,
};
use async_trait::async_trait;
use aws_sdk_dynamodb::{
    model::{
        AttributeDefinition, AttributeValue, Delete, KeySchemaElement, KeyType,
        ProvisionedThroughput, Put, ScalarAttributeType, TransactWriteItem,
    },
    output::QueryOutput,
    types::{Blob, SdkError},
    Client,
};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, mem, str::FromStr};
use thiserror::Error;

use static_assertions as sa;

/// The configuration to connect to DynamoDB.
pub type Config = aws_sdk_dynamodb::Config;

#[cfg(test)]
#[path = "unit_tests/dynamo_db_context_tests.rs"]
mod dynamo_db_context_tests;

/// The tag used for the journal stuff.
const JOURNAL_TAG: u8 = 0;
sa::const_assert!(JOURNAL_TAG < MIN_VIEW_TAG);

/// The attribute name of the partition key.
const PARTITION_ATTRIBUTE: &str = "item_partition";

/// A dummy value to use as the partition key.
const DUMMY_PARTITION_KEY: &[u8] = &[0];

/// The attribute name of the primary key (used as a sort key).
const KEY_ATTRIBUTE: &str = "item_key";

/// The attribute name of the table value blob.
const VALUE_ATTRIBUTE: &str = "item_value";

/// The attribute for obtaining the primary key (used as a sort key) with the stored value.
const KEY_VALUE_ATTRIBUTE: &str = "item_key, item_value";

/// Fundamental constants in DynamoDB: The maximum size of a value is 400KB
/// See https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/ServiceQuotas.html
const MAX_VALUE_BYTES: usize = 409600;

/// Fundamental constants in DynamoDB: The maximum size of a TransactWriteItem is 4M.
/// See https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_TransactWriteItems.html
const _MAX_TRANSACT_WRITE_ITEM_BYTES: usize = 4194304;

/// Fundamental constants in DynamoDB: The maximum size of a TransactWriteItem is 100.
/// See <https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_TransactWriteItems.html>
pub const MAX_TRANSACT_WRITE_ITEM_SIZE: usize = 100;

/// Fundamental constants in DynamoDB: The maximum size of a BatchWriteItem is 16M.
/// See <https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_BatchWriteItem.html>
const MAX_BATCH_WRITE_ITEM_BYTES: usize = 16777216;

/// Fundamental constant of DynamoDB: The maximum number of simultaneous connections is 50.
/// See https://stackoverflow.com/questions/13128613/amazon-dynamo-db-max-client-connections
const MAX_CONNECTIONS: usize = 50;

/// Builds the key attributes for a table item.
///
/// The key is composed of two attributes that are both binary blobs. The first attribute is a
/// partition key and is currently just a dummy value that ensures all items are in the same
/// partion. This is necessary for range queries to work correctly.
///
/// The second attribute is the actual key value, which is generated by concatenating the
/// context prefix. The Vec<u8> expression is obtained from self.derive_key.
fn build_key(key: Vec<u8>) -> HashMap<String, AttributeValue> {
    [
        (
            PARTITION_ATTRIBUTE.to_owned(),
            AttributeValue::B(Blob::new(DUMMY_PARTITION_KEY)),
        ),
        (KEY_ATTRIBUTE.to_owned(), AttributeValue::B(Blob::new(key))),
    ]
    .into()
}

/// Builds the value attribute for storing a table item.
fn build_key_value(key: Vec<u8>, value: Vec<u8>) -> HashMap<String, AttributeValue> {
    [
        (
            PARTITION_ATTRIBUTE.to_owned(),
            AttributeValue::B(Blob::new(DUMMY_PARTITION_KEY)),
        ),
        (KEY_ATTRIBUTE.to_owned(), AttributeValue::B(Blob::new(key))),
        (
            VALUE_ATTRIBUTE.to_owned(),
            AttributeValue::B(Blob::new(value)),
        ),
    ]
    .into()
}

/// Extracts the key attribute from an item.
fn extract_key(
    prefix_len: usize,
    attributes: &HashMap<String, AttributeValue>,
) -> Result<&[u8], DynamoDbContextError> {
    let key = attributes
        .get(KEY_ATTRIBUTE)
        .ok_or(DynamoDbContextError::MissingKey)?;
    match key {
        AttributeValue::B(blob) => Ok(&blob.as_ref()[prefix_len..]),
        key => Err(DynamoDbContextError::wrong_key_type(key)),
    }
}

/// Extracts the value attribute from an item.
fn extract_value(
    attributes: &HashMap<String, AttributeValue>,
) -> Result<&[u8], DynamoDbContextError> {
    // According to the official AWS DynamoDB documentation:
    // "Binary must have a length greater than zero if the attribute is used as a key attribute for a table or index"
    let value = attributes
        .get(VALUE_ATTRIBUTE)
        .ok_or(DynamoDbContextError::MissingValue)?;
    match value {
        AttributeValue::B(blob) => Ok(blob.as_ref()),
        value => Err(DynamoDbContextError::wrong_value_type(value)),
    }
}

/// Extracts the value attribute from an item (returned by value).
fn extract_value_owned(
    attributes: &mut HashMap<String, AttributeValue>,
) -> Result<Vec<u8>, DynamoDbContextError> {
    let value = attributes
        .remove(VALUE_ATTRIBUTE)
        .ok_or(DynamoDbContextError::MissingValue)?;
    match value {
        AttributeValue::B(blob) => Ok(blob.into_inner()),
        value => Err(DynamoDbContextError::wrong_value_type(&value)),
    }
}

/// Extracts the key and value attributes from an item.
fn extract_key_value(
    prefix_len: usize,
    attributes: &HashMap<String, AttributeValue>,
) -> Result<(&[u8], &[u8]), DynamoDbContextError> {
    let key = extract_key(prefix_len, attributes)?;
    let value = extract_value(attributes)?;
    Ok((key, value))
}

/// Extracts the `(key, value)` pair attributes from an item (returned by value).
fn extract_key_value_owned(
    prefix_len: usize,
    attributes: &mut HashMap<String, AttributeValue>,
) -> Result<(Vec<u8>, Vec<u8>), DynamoDbContextError> {
    let key = extract_key(prefix_len, attributes)?.to_vec();
    let value = extract_value_owned(attributes)?;
    Ok((key, value))
}

#[derive(Default)]
struct TransactionBuilder {
    transacts: Vec<TransactWriteItem>,
}

impl TransactionBuilder {
    fn insert_delete_request(
        &mut self,
        key: Vec<u8>,
        db: &DynamoDbClientInternal,
    ) -> Result<(), DynamoDbContextError> {
        if key.is_empty() {
            return Err(DynamoDbContextError::ZeroLengthKey);
        }
        let request = Delete::builder()
            .table_name(&db.table.0)
            .set_key(Some(build_key(key)))
            .build();
        let transact = TransactWriteItem::builder().delete(request).build();
        self.transacts.push(transact);
        Ok(())
    }

    fn insert_put_request(
        &mut self,
        key: Vec<u8>,
        value: Vec<u8>,
        db: &DynamoDbClientInternal,
    ) -> Result<(), DynamoDbContextError> {
        if key.is_empty() {
            return Err(DynamoDbContextError::ZeroLengthKey);
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(DynamoDbContextError::ValueLengthTooLarge);
        }
        let request = Put::builder()
            .table_name(&db.table.0)
            .set_item(Some(build_key_value(key, value)))
            .build();
        let transact = TransactWriteItem::builder().put(request).build();
        self.transacts.push(transact);
        Ok(())
    }

    async fn submit(self, db: &DynamoDbClientInternal) -> Result<(), DynamoDbContextError> {
        if self.transacts.len() > MAX_TRANSACT_WRITE_ITEM_SIZE {
            return Err(DynamoDbContextError::TransactUpperLimitSize);
        }
        if !self.transacts.is_empty() {
            db.client
                .transact_write_items()
                .set_transact_items(Some(self.transacts))
                .send()
                .await?;
        }
        // Drop the output of type TransactWriteItemsOutput
        Ok(())
    }
}

#[repr(u8)]
enum KeyTag {
    /// Prefix for the storing of the header of the journal.
    Journal = 1,
    /// Prefix for the block entry.
    Entry,
}

fn get_journaling_key(base_key: &[u8], tag: u8, pos: u32) -> Result<Vec<u8>, DynamoDbContextError> {
    // We used the value 0 because it does not collide with other key values.
    // since other tags are starting from 1.
    let mut key = base_key.to_vec();
    key.extend([JOURNAL_TAG]);
    key.extend([tag]);
    bcs::serialize_into(&mut key, &pos)?;
    Ok(key)
}

/// The header that contains the current state of the journal.
#[derive(Serialize, Deserialize)]
struct JournalHeader {
    block_count: u32,
}

impl JournalHeader {
    /// Resolves the database by using the header that has been retrieved
    async fn coherently_resolve_journal(
        mut self,
        db: &DynamoDbClientInternal,
        base_key: &[u8],
    ) -> Result<(), DynamoDbContextError> {
        loop {
            if self.block_count == 0 {
                break;
            }
            let key = get_journaling_key(base_key, KeyTag::Entry as u8, self.block_count - 1)?;
            let value = db.read_key::<DynamoDbBatch>(&key).await?;
            if let Some(value) = value {
                let mut tb = TransactionBuilder::default();
                tb.insert_delete_request(key, db)?; // Delete the preceding journal entry
                for delete in value.0.deletions {
                    tb.insert_delete_request(delete, db)?;
                }
                for key_value in value.0.insertions {
                    tb.insert_put_request(key_value.0, key_value.1, db)?;
                }
                self.block_count -= 1;
                DynamoDbBatch::add_journal_header_operations(&mut tb, &self, db, base_key)?;
                tb.submit(db).await?;
            } else {
                return Err(DynamoDbContextError::DatabaseRecoveryFailed);
            }
        }
        Ok(())
    }
}

/// The bunch of deletes and writes to be done.
#[derive(Serialize, Deserialize)]
struct DynamoDbBatch(SimpleUnorderedBatch);

impl DynamoDbBatch {
    /// The total number of entries to be submitted
    fn len(&self) -> usize {
        self.0.deletions.len() + self.0.insertions.len()
    }

    fn is_fastpath_feasible(&self) -> bool {
        self.len() <= MAX_TRANSACT_WRITE_ITEM_SIZE
    }

    fn add_journal_header_operations(
        transact_builder: &mut TransactionBuilder,
        header: &JournalHeader,
        db: &DynamoDbClientInternal,
        base_key: &[u8],
    ) -> Result<(), DynamoDbContextError> {
        let key = get_journaling_key(base_key, KeyTag::Journal as u8, 0)?;
        if header.block_count > 0 {
            let value = bcs::to_bytes(header)?;
            transact_builder.insert_put_request(key, value, db)?;
        } else {
            transact_builder.insert_delete_request(key, db)?;
        }
        Ok(())
    }

    /// Writes blocks to the database and resolves them later.
    pub async fn write_journal(
        self,
        db: &DynamoDbClientInternal,
        base_key: &[u8],
    ) -> Result<JournalHeader, DynamoDbContextError> {
        let delete_count = self.0.deletions.len();
        let insert_count = self.0.insertions.len();
        let total_count = delete_count + insert_count;
        let mut curr_size = 0;
        let mut curr_len = 0;
        let mut deletions = Vec::new();
        let mut insertions = Vec::new();
        let mut block_count = 0;
        for i in 0..total_count {
            curr_len += 1;
            if i < delete_count {
                let delete = &self.0.deletions[i];
                curr_size += delete.len();
                deletions.push(delete.to_vec());
            } else {
                let key_value = &self.0.insertions[i - delete_count];
                curr_size += key_value.0.len() + key_value.1.len();
                insertions.push(key_value.clone());
            }
            let do_flush = if i == total_count - 1 || curr_len == MAX_TRANSACT_WRITE_ITEM_SIZE - 2 {
                true
            } else {
                let size_next = if i + 1 < delete_count {
                    self.0.deletions[i + 1].len()
                } else {
                    let key_value = &self.0.insertions[i + 1 - delete_count];
                    key_value.0.len() + key_value.1.len()
                };
                curr_size + size_next > MAX_BATCH_WRITE_ITEM_BYTES
            };
            if do_flush {
                let simple_unordered_batch = SimpleUnorderedBatch {
                    deletions: mem::take(&mut deletions),
                    insertions: mem::take(&mut insertions),
                };
                let entry = DynamoDbBatch(simple_unordered_batch);
                let key = get_journaling_key(base_key, KeyTag::Entry as u8, block_count)?;
                let value = bcs::to_bytes(&entry)?;
                db.write_single_key_value(key, value).await?;
                block_count += 1;
                curr_size = 0;
                curr_len = 0;
            }
        }
        let header = JournalHeader { block_count };
        if block_count > 0 {
            let key = get_journaling_key(base_key, KeyTag::Journal as u8, 0)?;
            let value = bcs::to_bytes(&header)?;
            db.write_single_key_value(key, value).await?;
        }
        Ok(header)
    }

    /// This code is for submitting the transaction in one single transaction when that is possible.
    pub async fn write_fastpath_failsafe(
        self,
        db: &DynamoDbClientInternal,
    ) -> Result<(), DynamoDbContextError> {
        let mut tb = TransactionBuilder::default();
        for key in self.0.deletions {
            tb.insert_delete_request(key, db)?;
        }
        for key_value in self.0.insertions {
            tb.insert_put_request(key_value.0, key_value.1, db)?;
        }
        tb.submit(db).await
    }

    async fn from_batch(
        db: &DynamoDbClientInternal,
        batch: Batch,
    ) -> Result<Self, DynamoDbContextError> {
        // The DynamoDB does not support the `DeletePrefix` operation.
        // Therefore it does not make sense to have a delete prefix and they have to
        // be downloaded for making a list.
        // Also we remove the deletes that are followed by inserts on the same key because
        // the TransactWriteItem and BatchWriteItem are not going to work that way.
        let unordered_batch = batch.simplify();
        let simple_unordered_batch = unordered_batch.expand_delete_prefixes(db).await?;
        Ok(DynamoDbBatch(simple_unordered_batch))
    }
}

// Inspired by https://depth-first.com/articles/2020/06/22/returning-rust-iterators/
#[doc(hidden)]
pub struct DynamoDbKeyIterator<'a> {
    prefix_len: usize,
    iter: std::iter::Flatten<
        std::option::Iter<'a, Vec<HashMap<std::string::String, AttributeValue>>>,
    >,
}

impl<'a> Iterator for DynamoDbKeyIterator<'a> {
    type Item = Result<&'a [u8], DynamoDbContextError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|x| extract_key(self.prefix_len, x))
    }
}

/// A set of keys returned by a search query on DynamoDB.
pub struct DynamoDbKeys {
    prefix_len: usize,
    response: Box<QueryOutput>,
}

impl KeyIterable<DynamoDbContextError> for DynamoDbKeys {
    type Iterator<'a> = DynamoDbKeyIterator<'a> where Self: 'a;

    fn iterator(&self) -> Self::Iterator<'_> {
        DynamoDbKeyIterator {
            prefix_len: self.prefix_len,
            iter: self.response.items.iter().flatten(),
        }
    }
}

// Inspired by https://depth-first.com/articles/2020/06/22/returning-rust-iterators/
#[doc(hidden)]
pub struct DynamoDbKeyValueIterator<'a> {
    prefix_len: usize,
    iter: std::iter::Flatten<
        std::option::Iter<'a, Vec<HashMap<std::string::String, AttributeValue>>>,
    >,
}

impl<'a> Iterator for DynamoDbKeyValueIterator<'a> {
    type Item = Result<(&'a [u8], &'a [u8]), DynamoDbContextError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()
            .map(|x| extract_key_value(self.prefix_len, x))
    }
}

#[doc(hidden)]
pub struct DynamoDbKeyValueIteratorOwned {
    prefix_len: usize,
    iter: std::iter::Flatten<
        std::option::IntoIter<Vec<HashMap<std::string::String, AttributeValue>>>,
    >,
}

impl Iterator for DynamoDbKeyValueIteratorOwned {
    type Item = Result<(Vec<u8>, Vec<u8>), DynamoDbContextError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()
            .map(|mut x| extract_key_value_owned(self.prefix_len, &mut x))
    }
}

/// A set of `(key, value)` returned by a search query on DynamoDb.
pub struct DynamoDbKeyValues {
    prefix_len: usize,
    response: Box<QueryOutput>,
}

impl KeyValueIterable<DynamoDbContextError> for DynamoDbKeyValues {
    type Iterator<'a> = DynamoDbKeyValueIterator<'a> where Self: 'a;
    type IteratorOwned = DynamoDbKeyValueIteratorOwned;

    fn iterator(&self) -> Self::Iterator<'_> {
        DynamoDbKeyValueIterator {
            prefix_len: self.prefix_len,
            iter: self.response.items.iter().flatten(),
        }
    }

    fn into_iterator_owned(self) -> Self::IteratorOwned {
        DynamoDbKeyValueIteratorOwned {
            prefix_len: self.prefix_len,
            iter: self.response.items.into_iter().flatten(),
        }
    }
}

/// A DynamoDB client.
#[derive(Debug, Clone)]
pub struct DynamoDbClientInternal {
    client: Client,
    table: TableName,
}

#[async_trait]
impl DeletePrefixExpander for DynamoDbClientInternal {
    type Error = DynamoDbContextError;
    async fn expand_delete_prefix(&self, key_prefix: &[u8]) -> Result<Vec<Vec<u8>>, Self::Error> {
        let mut vector_list = Vec::new();
        for key in self.find_keys_by_prefix(key_prefix).await?.iterator() {
            vector_list.push(key?.to_vec());
        }
        Ok(vector_list)
    }
}

impl DynamoDbClientInternal {
    /// Creates a new [`DynamoDbClientInternal`] instance using the provided `config` parameters.
    pub async fn from_config(
        config: impl Into<Config>,
        table: TableName,
    ) -> Result<(Self, TableStatus), DynamoDbContextError> {
        let db = DynamoDbClientInternal {
            client: Client::from_conf(config.into()),
            table,
        };

        let table_status = db.create_table_if_needed().await?;

        Ok((db, table_status))
    }

    async fn get_query_output(
        &self,
        attribute_str: &str,
        key_prefix: &[u8],
    ) -> Result<QueryOutput, DynamoDbContextError> {
        let response = self
            .client
            .query()
            .table_name(self.table.as_ref())
            .projection_expression(attribute_str)
            .key_condition_expression(format!(
                "{PARTITION_ATTRIBUTE} = :partition and begins_with({KEY_ATTRIBUTE}, :prefix)"
            ))
            .expression_attribute_values(
                ":partition",
                AttributeValue::B(Blob::new(DUMMY_PARTITION_KEY)),
            )
            .expression_attribute_values(":prefix", AttributeValue::B(Blob::new(key_prefix)))
            .send()
            .await?;
        Ok(response)
    }

    async fn read_key_bytes_general(
        &self,
        key_db: HashMap<String, AttributeValue>,
    ) -> Result<Option<Vec<u8>>, DynamoDbContextError> {
        let response = self
            .client
            .get_item()
            .table_name(self.table.as_ref())
            .set_key(Some(key_db))
            .send()
            .await?;

        match response.item {
            Some(mut item) => {
                let value = extract_value_owned(&mut item)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    async fn write_single_key_value(
        &self,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<(), DynamoDbContextError> {
        let mut tb = TransactionBuilder::default();
        tb.insert_put_request(key, value, self)?;
        tb.submit(self).await
    }

    /// Creates the storage table if it doesn't exist.
    ///
    /// Attempts to create the table and ignores errors that indicate that it already exists.
    async fn create_table_if_needed(&self) -> Result<TableStatus, DynamoDbContextError> {
        let result = self
            .client
            .create_table()
            .table_name(self.table.as_ref())
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(PARTITION_ATTRIBUTE)
                    .attribute_type(ScalarAttributeType::B)
                    .build(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(KEY_ATTRIBUTE)
                    .attribute_type(ScalarAttributeType::B)
                    .build(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(PARTITION_ATTRIBUTE)
                    .key_type(KeyType::Hash)
                    .build(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(KEY_ATTRIBUTE)
                    .key_type(KeyType::Range)
                    .build(),
            )
            .provisioned_throughput(
                ProvisionedThroughput::builder()
                    .read_capacity_units(10)
                    .write_capacity_units(10)
                    .build(),
            )
            .send()
            .await;

        match result {
            Ok(_) => Ok(TableStatus::New),
            Err(error) if error.is_resource_in_use_exception() => Ok(TableStatus::Existing),
            Err(error) => Err(error.into()),
        }
    }
}

#[async_trait]
impl KeyValueStoreClient for DynamoDbClientInternal {
    const MAX_CONNECTIONS: usize = MAX_CONNECTIONS;
    type Error = DynamoDbContextError;
    type Keys = DynamoDbKeys;
    type KeyValues = DynamoDbKeyValues;

    async fn read_key_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DynamoDbContextError> {
        let key_db = build_key(key.to_vec());
        self.read_key_bytes_general(key_db).await
    }

    async fn read_multi_key_bytes(
        &self,
        keys: Vec<Vec<u8>>,
    ) -> Result<Vec<Option<Vec<u8>>>, DynamoDbContextError> {
        let mut handles = Vec::new();
        for key in keys {
            let key_db = build_key(key);
            let handle = self.read_key_bytes_general(key_db);
            handles.push(handle);
        }
        let result = join_all(handles).await;
        Ok(result.into_iter().collect::<Result<_, _>>()?)
    }

    // TODO(#201): Large responses may be truncated.
    async fn find_keys_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<DynamoDbKeys, DynamoDbContextError> {
        if key_prefix.is_empty() {
            return Err(DynamoDbContextError::ZeroLengthKeyPrefix);
        }
        let response = Box::new(self.get_query_output(KEY_ATTRIBUTE, key_prefix).await?);
        Ok(DynamoDbKeys {
            prefix_len: key_prefix.len(),
            response,
        })
    }

    // TODO(#201): Large responses may be truncated.
    async fn find_key_values_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<DynamoDbKeyValues, DynamoDbContextError> {
        if key_prefix.is_empty() {
            return Err(DynamoDbContextError::ZeroLengthKeyPrefix);
        }
        let response = Box::new(
            self.get_query_output(KEY_VALUE_ATTRIBUTE, key_prefix)
                .await?,
        );
        Ok(DynamoDbKeyValues {
            prefix_len: key_prefix.len(),
            response,
        })
    }

    async fn write_batch(&self, batch: Batch, base_key: &[u8]) -> Result<(), DynamoDbContextError> {
        let block_operations = DynamoDbBatch::from_batch(self, batch).await?;
        if block_operations.is_fastpath_feasible() {
            block_operations.write_fastpath_failsafe(self).await
        } else {
            let header = block_operations.write_journal(self, base_key).await?;
            header.coherently_resolve_journal(self, base_key).await
        }
    }

    async fn clear_journal(&self, base_key: &[u8]) -> Result<(), DynamoDbContextError> {
        let key = get_journaling_key(base_key, KeyTag::Journal as u8, 0)?;
        let value = self.read_key::<JournalHeader>(&key).await?;
        if let Some(header) = value {
            header.coherently_resolve_journal(self, base_key).await?;
        }
        Ok(())
    }
}

/// A shared DB client for DynamoDb implementing LruCaching
#[derive(Clone)]
pub struct DynamoDbClient {
    client: LruCachingKeyValueClient<DynamoDbClientInternal>,
}

#[async_trait]
impl KeyValueStoreClient for DynamoDbClient {
    const MAX_CONNECTIONS: usize = MAX_CONNECTIONS;
    type Error = DynamoDbContextError;
    type Keys = DynamoDbKeys;
    type KeyValues = DynamoDbKeyValues;

    async fn read_key_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DynamoDbContextError> {
        self.client.read_key_bytes(key).await
    }

    async fn read_multi_key_bytes(
        &self,
        key: Vec<Vec<u8>>,
    ) -> Result<Vec<Option<Vec<u8>>>, DynamoDbContextError> {
        self.client.read_multi_key_bytes(key).await
    }

    async fn find_keys_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Self::Keys, DynamoDbContextError> {
        self.client.find_keys_by_prefix(key_prefix).await
    }

    async fn find_key_values_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Self::KeyValues, DynamoDbContextError> {
        self.client.find_key_values_by_prefix(key_prefix).await
    }

    async fn write_batch(&self, batch: Batch, base_key: &[u8]) -> Result<(), DynamoDbContextError> {
        self.client.write_batch(batch, base_key).await
    }

    async fn clear_journal(&self, base_key: &[u8]) -> Result<(), Self::Error> {
        self.client.clear_journal(base_key).await
    }
}

impl DynamoDbClient {
    /// Creation of the DynamoDbClient with an LRU caching
    pub async fn from_config(
        config: impl Into<Config>,
        table: TableName,
        cache_size: usize,
    ) -> Result<(Self, TableStatus), DynamoDbContextError> {
        let (client, table_name) = DynamoDbClientInternal::from_config(config, table).await?;
        Ok((
            Self {
                client: LruCachingKeyValueClient::new(client, cache_size),
            },
            table_name,
        ))
    }
}

impl DynamoDbClient {
    /// Creates a new [`DynamoDbClientInternal`] instance.
    pub async fn new(
        table: TableName,
        cache_size: usize,
    ) -> Result<(Self, TableStatus), DynamoDbContextError> {
        let config = aws_config::load_from_env().await;
        DynamoDbClient::from_config(&config, table, cache_size).await
    }

    /// Creates a new [`DynamoDbClientInternal`] instance using a LocalStack endpoint.
    ///
    /// Requires a `LOCALSTACK_ENDPOINT` environment variable with the endpoint address to connect
    /// to the LocalStack instance. Creates the table if it doesn't exist yet, reporting a
    /// [`TableStatus`] to indicate if the table was created or if it already exists.
    pub async fn with_localstack(
        table: TableName,
        cache_size: usize,
    ) -> Result<(Self, TableStatus), DynamoDbContextError> {
        let base_config = aws_config::load_from_env().await;
        let config = aws_sdk_dynamodb::config::Builder::from(&base_config)
            .endpoint_resolver(localstack::get_endpoint()?)
            .build();
        DynamoDbClient::from_config(config, table, cache_size).await
    }
}

/// An implementation of [`Context`][trait1] based on [`DynamoDbClient`].
///
/// [trait1]: crate::common::Context
pub type DynamoDbContext<E> = ContextFromDb<E, DynamoDbClient>;

impl<E> DynamoDbContext<E>
where
    E: Clone + Sync + Send,
{
    fn create_context(
        db_tablestatus: (DynamoDbClient, TableStatus),
        base_key: Vec<u8>,
        extra: E,
    ) -> (Self, TableStatus) {
        let storage = DynamoDbContext {
            db: db_tablestatus.0,
            base_key,
            extra,
        };
        (storage, db_tablestatus.1)
    }

    /// Creates a new [`DynamoDbContext`] instance.
    pub async fn new(
        table: TableName,
        cache_size: usize,
        base_key: Vec<u8>,
        extra: E,
    ) -> Result<(Self, TableStatus), DynamoDbContextError> {
        let db_tablestatus = DynamoDbClient::new(table, cache_size).await?;
        Ok(Self::create_context(db_tablestatus, base_key, extra))
    }

    /// Creates a new [`DynamoDbContext`] instance from the given AWS configuration.
    pub async fn from_config(
        config: impl Into<Config>,
        table: TableName,
        cache_size: usize,
        base_key: Vec<u8>,
        extra: E,
    ) -> Result<(Self, TableStatus), DynamoDbContextError> {
        let db_tablestatus = DynamoDbClient::from_config(config, table, cache_size).await?;
        Ok(Self::create_context(db_tablestatus, base_key, extra))
    }

    /// Creates a new [`DynamoDbContext`] instance using a LocalStack endpoint.
    ///
    /// Requires a `LOCALSTACK_ENDPOINT` environment variable with the endpoint address to connect
    /// to the LocalStack instance. Creates the table if it doesn't exist yet, reporting a
    /// [`TableStatus`] to indicate if the table was created or if it already exists.
    pub async fn with_localstack(
        table: TableName,
        cache_size: usize,
        base_key: Vec<u8>,
        extra: E,
    ) -> Result<(Self, TableStatus), DynamoDbContextError> {
        let db_tablestatus = DynamoDbClient::with_localstack(table, cache_size).await?;
        Ok(Self::create_context(db_tablestatus, base_key, extra))
    }
}

/// Status of a table at the creation time of a [`DynamoDbContext`] instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableStatus {
    /// Table was created during the construction of the [`DynamoDbContext`] instance.
    New,
    /// Table already existed when the [`DynamoDbContext`] instance was created.
    Existing,
}

/// A DynamoDB table name.
///
/// Table names must follow some [naming
/// rules](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/HowItWorks.NamingRulesDataTypes.html#HowItWorks.NamingRules),
/// so this type ensures that they are properly validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableName(String);

impl FromStr for TableName {
    type Err = InvalidTableName;

    fn from_str(string: &str) -> Result<Self, Self::Err> {
        if string.len() < 3 {
            return Err(InvalidTableName::TooShort);
        }
        if string.len() > 255 {
            return Err(InvalidTableName::TooLong);
        }
        if !string.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '.'
                || character == '-'
                || character == '_'
        }) {
            return Err(InvalidTableName::InvalidCharacter);
        }
        Ok(TableName(string.to_owned()))
    }
}

impl AsRef<String> for TableName {
    fn as_ref(&self) -> &String {
        &self.0
    }
}

/// Error when validating a table name.
#[derive(Debug, Error)]
pub enum InvalidTableName {
    /// The table name should be at least 3 characters.
    #[error("Table name must have at least 3 characters")]
    TooShort,

    /// The table name should be at most 63 characters.
    #[error("Table name must be at most 63 characters")]
    TooLong,

    /// allowed characters are lowercase letters, numbers, periods and hyphens
    #[error("Table name must only contain lowercase letters, numbers, periods and hyphens")]
    InvalidCharacter,
}

/// Errors that occur when using [`DynamoDbContext`].
#[derive(Debug, Error)]
pub enum DynamoDbContextError {
    /// An error occurred while getting the item.
    #[error(transparent)]
    Get(#[from] Box<SdkError<aws_sdk_dynamodb::error::GetItemError>>),

    /// An error occurred while writing a batch of items.
    #[error(transparent)]
    BatchWriteItem(#[from] Box<SdkError<aws_sdk_dynamodb::error::BatchWriteItemError>>),

    /// An error occurred while writing a transaction of items.
    #[error(transparent)]
    TransactWriteItem(#[from] Box<SdkError<aws_sdk_dynamodb::error::TransactWriteItemsError>>),

    /// An error occurred while doing a Query.
    #[error(transparent)]
    Query(#[from] Box<SdkError<aws_sdk_dynamodb::error::QueryError>>),

    /// The transact maximum size is MAX_TRANSACT_WRITE_ITEM_SIZE.
    #[error("The transact must have length at most MAX_TRANSACT_WRITE_ITEM_SIZE")]
    TransactUpperLimitSize,

    /// Keys have to be of non-zero length.
    #[error("The key must be of strictly positive length")]
    ZeroLengthKey,

    /// Key prefixes have to be of non-zero length.
    #[error("The key_prefix must be of strictly positive length")]
    ZeroLengthKeyPrefix,

    /// The recovery failed.
    #[error("The DynamoDB database recovery failed")]
    DatabaseRecoveryFailed,

    /// The length of the value should be at most 400KB.
    #[error("The DynamoDB value should be less than 400KB")]
    ValueLengthTooLarge,

    /// The stored key is missing.
    #[error("The stored key attribute is missing")]
    MissingKey,

    /// The type of the keys was not correct (It should have been a binary blob).
    #[error("Key was stored as {0}, but it was expected to be stored as a binary blob")]
    WrongKeyType(String),

    /// The value attribute is missing.
    #[error("The stored value attribute is missing")]
    MissingValue,

    /// The value was stored as the wrong type (it should be a binary blob).
    #[error("Value was stored as {0}, but it was expected to be stored as a binary blob")]
    WrongValueType(String),

    /// A BCS error occurred.
    #[error(transparent)]
    BcsError(#[from] bcs::Error),

    /// An Endpoint error occurred.
    #[error(transparent)]
    Endpoint(#[from] localstack::EndpointError),

    /// An error occurred while creating the table.
    #[error(transparent)]
    CreateTable(#[from] SdkError<aws_sdk_dynamodb::error::CreateTableError>),
}

impl<InnerError> From<SdkError<InnerError>> for DynamoDbContextError
where
    DynamoDbContextError: From<Box<SdkError<InnerError>>>,
{
    fn from(error: SdkError<InnerError>) -> Self {
        Box::new(error).into()
    }
}

impl DynamoDbContextError {
    /// Creates a [`DynamoDbContextError::WrongKeyType`] instance based on the returned value type.
    ///
    /// # Panics
    ///
    /// If the value type is in the correct type, a binary blob.
    pub fn wrong_key_type(value: &AttributeValue) -> Self {
        DynamoDbContextError::WrongKeyType(Self::type_description_of(value))
    }

    /// Creates a [`DynamoDbContextError::WrongValueType`] instance based on the returned value type.
    ///
    /// # Panics
    ///
    /// If the value type is in the correct type, a binary blob.
    pub fn wrong_value_type(value: &AttributeValue) -> Self {
        DynamoDbContextError::WrongValueType(Self::type_description_of(value))
    }

    fn type_description_of(value: &AttributeValue) -> String {
        match value {
            AttributeValue::B(_) => unreachable!("creating an error type for the correct type"),
            AttributeValue::Bool(_) => "a boolean",
            AttributeValue::Bs(_) => "a list of binary blobs",
            AttributeValue::L(_) => "a list",
            AttributeValue::M(_) => "a map",
            AttributeValue::N(_) => "a number",
            AttributeValue::Ns(_) => "a list of numbers",
            AttributeValue::Null(_) => "a null value",
            AttributeValue::S(_) => "a string",
            AttributeValue::Ss(_) => "a list of strings",
            _ => "an unknown type",
        }
        .to_owned()
    }
}

impl From<DynamoDbContextError> for crate::views::ViewError {
    fn from(error: DynamoDbContextError) -> Self {
        Self::ContextError {
            backend: "DynamoDB".to_string(),
            error: error.to_string(),
        }
    }
}

/// A helper trait to add a `SdkError<CreateTableError>::is_resource_in_use_exception()` method.
trait IsResourceInUseException {
    /// Checks if the error is a resource is in use exception.
    fn is_resource_in_use_exception(&self) -> bool;
}

impl IsResourceInUseException for SdkError<aws_sdk_dynamodb::error::CreateTableError> {
    fn is_resource_in_use_exception(&self) -> bool {
        matches!(
            self,
            SdkError::ServiceError {
                err: aws_sdk_dynamodb::error::CreateTableError {
                    kind: aws_sdk_dynamodb::error::CreateTableErrorKind::ResourceInUseException(_),
                    ..
                },
                ..
            }
        )
    }
}
