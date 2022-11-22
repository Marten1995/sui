// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Storage Atomicity Layer Library (aka Sally) is a wrapper around pluggable storage backends
//! which implement a common key value interface. It enables users to switch storage backends
//! in their code with simple options. It is also designed to be able to support atomic operations
//! across different columns of the db even when they are backed by different storage instances.
//!
//! # Examples
//!
//! ```
//! use typed_store::rocks::*;
//! use typed_store::test_db::*;
//! use typed_store::sally::SallyDBOptions;
//! use typed_store_derive::SallyDB;
//! use typed_store::sally::SallyColumn;
//! use typed_store::traits::TypedStoreDebug;
//! use crate::typed_store::Map;
//!
//! // `ExampleTable` is a sally db instance where each column is first initialized with TestDB
//! // (btree map) backend and later switched to a RocksDB column family
//!
//! #[derive(SallyDB)]
//! pub struct ExampleTable {
//!   col1: SallyColumn<String, String>,
//!   col2: SallyColumn<i32, String>,
//! }
//!
//! fn update_table(table: &ExampleTable) {
//!     table.col1.insert(&"key1".to_string(), &"value1".to_string());
//!     table.col2.insert(&1, &"test".to_string());
//!     // create a write batch and do atomic commit
//!     let keys_vals = (1..100).map(|i| (i, i.to_string()));
//!     let mut wb = table.col1.batch();
//!     wb = wb.insert_batch(&table.col2, keys_vals).expect("Failed to batch insert");
//!     wb = wb.delete_range(&table.col2, &50, &100).expect("Failed to batch delete");
//!     wb.write();
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), TypedStoreError> {
//!     // use a btree map backend first
//!     let mut table = ExampleTable::init(SallyDBOptions::TestDB);
//!     update_table(&table);
//!     let primary_path = tempfile::tempdir().expect("Failed to open path").into_path();
//!     // switch to rocksdb backend
//!     table = ExampleTable::init(SallyDBOptions::RocksDB((primary_path, RocksDBAccessType::Primary, None, None)));
//!     update_table(&table);
//!     Ok(())
//! }
//! ```
use crate::{
    rocks::{
        default_db_options, keys::Keys, values::Values, DBBatch, DBMap, DBOptions,
        RocksDBAccessType, TypedStoreError,
    },
    test_db::{TestDB, TestDBIter, TestDBKeys, TestDBValues, TestDBWriteBatch},
    traits::Map,
};

use crate::rocks::iter::Iter as RocksDBIter;
use crate::rocks::DBMapTableConfigMap;
use collectable::TryExtend;
use rocksdb::Options;
use serde::{de::DeserializeOwned, Serialize};
use std::borrow::Borrow;
use std::{collections::BTreeMap, path::PathBuf};

pub enum SallyRunMode {
    // Whether Sally should use its own memtable and wal for read/write or just fallback to
    // reading/writing directly from the backend db. When columns in the db are backed by different
    // backend stores, we should never use `FallbackToDB` as that would lose atomicity,
    // transactions and db recovery
    FallbackToDB,
}

pub struct SallyConfig {
    pub mode: SallyRunMode,
}

impl Default for SallyConfig {
    fn default() -> Self {
        Self {
            mode: SallyRunMode::FallbackToDB,
        }
    }
}

/// A Sally column could be anything that implements key value interface. We will eventually have
/// Sally serve read/writes using its own memtable and wal when columns in the db are backend by more then
/// one backend store (e.g different rocksdb instances and/or distributed key value stores)
pub enum SallyColumn<K, V> {
    RocksDB((DBMap<K, V>, SallyConfig)),
    TestDB((TestDB<K, V>, SallyConfig)),
}

impl<K, V> SallyColumn<K, V> {
    pub fn new_single_rocksdb(db: DBMap<K, V>) -> Self {
        // When all columns in the db are backed by a single rocksdb instance, we will fallback to
        // using native rocksdb read and write apis and use default config
        SallyColumn::RocksDB((db, SallyConfig::default()))
    }
    pub fn new_testdb(db: TestDB<K, V>) -> Self {
        SallyColumn::TestDB((db, SallyConfig::default()))
    }
    pub fn batch(&self) -> SallyWriteBatch {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => SallyWriteBatch::RocksDB(db_map.batch()),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => SallyWriteBatch::TestDB(test_db.batch()),
        }
    }
}

impl<'a, K, V> Map<'a, K, V> for SallyColumn<K, V>
where
    K: Serialize + DeserializeOwned,
    V: Serialize + DeserializeOwned,
{
    type Error = TypedStoreError;
    type Iterator = SallyIter<'a, K, V>;
    type Keys = SallyKeys<'a, K>;
    type Values = SallyValues<'a, V>;

    fn contains_key(&self, key: &K) -> Result<bool, TypedStoreError> {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.contains_key(key),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.contains_key(key),
        }
    }
    fn get(&self, key: &K) -> Result<Option<V>, TypedStoreError> {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.get(key),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.get(key),
        }
    }
    fn get_raw_bytes(&self, key: &K) -> Result<Option<Vec<u8>>, TypedStoreError> {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.get_raw_bytes(key),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.get_raw_bytes(key),
        }
    }
    fn insert(&self, key: &K, value: &V) -> Result<(), TypedStoreError> {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.insert(key, value),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.insert(key, value),
        }
    }
    fn remove(&self, key: &K) -> Result<(), TypedStoreError> {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.remove(key),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.remove(key),
        }
    }
    fn clear(&self) -> Result<(), TypedStoreError> {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.clear(),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.clear(),
        }
    }
    fn is_empty(&self) -> bool {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.is_empty(),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.is_empty(),
        }
    }
    fn iter(&'a self) -> Self::Iterator {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => SallyIter::RocksDB(db_map.iter()),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => SallyIter::TestDB(test_db.iter()),
        }
    }
    fn keys(&'a self) -> Self::Keys {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => SallyKeys::RocksDB(db_map.keys()),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => SallyKeys::TestDB(test_db.keys()),
        }
    }
    fn values(&'a self) -> Self::Values {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => SallyValues::RocksDB(db_map.values()),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => SallyValues::TestDB(test_db.values()),
        }
    }
    fn multi_get<J>(
        &self,
        keys: impl IntoIterator<Item = J>,
    ) -> Result<Vec<Option<V>>, TypedStoreError>
    where
        J: Borrow<K>,
    {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.multi_get(keys),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.multi_get(keys),
        }
    }
    fn multi_insert<J, U>(
        &self,
        key_val_pairs: impl IntoIterator<Item = (J, U)>,
    ) -> Result<(), Self::Error>
    where
        J: Borrow<K>,
        U: Borrow<V>,
    {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.multi_insert(key_val_pairs),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.multi_insert(key_val_pairs),
        }
    }
    fn multi_remove<J>(&self, keys: impl IntoIterator<Item = J>) -> Result<(), Self::Error>
    where
        J: Borrow<K>,
    {
        self.batch().delete_batch(self, keys)?.write()
    }
    fn try_catch_up_with_primary(&self) -> Result<(), Self::Error> {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => Ok(db_map.try_catch_up_with_primary()?),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => Ok(test_db.try_catch_up_with_primary()?),
        }
    }
}

impl<J, K, U, V> TryExtend<(J, U)> for SallyColumn<K, V>
where
    J: Borrow<K> + std::clone::Clone,
    U: Borrow<V> + std::clone::Clone,
    K: Serialize,
    V: Serialize,
{
    type Error = TypedStoreError;

    fn try_extend<T>(&mut self, iter: &mut T) -> Result<(), Self::Error>
    where
        T: Iterator<Item = (J, U)>,
    {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.try_extend(iter),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.try_extend(iter),
        }
    }
    fn try_extend_from_slice(&mut self, slice: &[(J, U)]) -> Result<(), Self::Error> {
        match self {
            SallyColumn::RocksDB((
                db_map,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => db_map.try_extend_from_slice(slice),
            SallyColumn::TestDB((
                test_db,
                SallyConfig {
                    mode: SallyRunMode::FallbackToDB,
                },
            )) => test_db.try_extend_from_slice(slice),
        }
    }
}

/// A Sally write batch provides a mutable struct which holds a collection of db mutation operations and
/// applies them atomically to the db.
pub enum SallyWriteBatch {
    // Write batch for RocksDB backend when `fallback_to_db` is set as true
    RocksDB(DBBatch),
    // Write batch for btree map based backend
    TestDB(TestDBWriteBatch),
}

impl SallyWriteBatch {
    pub fn write(self) -> Result<(), TypedStoreError> {
        match self {
            SallyWriteBatch::RocksDB(db_batch) => db_batch.write(),
            SallyWriteBatch::TestDB(write_batch) => write_batch.write(),
        }
    }
    /// Deletes a set of keys given as an iterator
    pub fn delete_batch<J: Borrow<K>, K: Serialize, V>(
        self,
        db: &SallyColumn<K, V>,
        purged_vals: impl IntoIterator<Item = J>,
    ) -> Result<Self, TypedStoreError> {
        match (self, db) {
            (SallyWriteBatch::RocksDB(db_batch), SallyColumn::RocksDB((db_map, _))) => db_batch
                .delete_batch(db_map, purged_vals)
                .map(SallyWriteBatch::RocksDB),
            (SallyWriteBatch::TestDB(write_batch), SallyColumn::TestDB((test_db, _))) => {
                write_batch
                    .delete_batch(test_db, purged_vals)
                    .map(SallyWriteBatch::TestDB)
            }
            _ => unimplemented!(),
        }
    }
    /// Deletes a range of keys between `from` (inclusive) and `to` (non-inclusive)
    pub fn delete_range<'a, K: Serialize, V>(
        self,
        db: &'a SallyColumn<K, V>,
        from: &K,
        to: &K,
    ) -> Result<Self, TypedStoreError> {
        match (self, db) {
            (SallyWriteBatch::RocksDB(db_batch), SallyColumn::RocksDB((db_map, _))) => db_batch
                .delete_range(db_map, from, to)
                .map(SallyWriteBatch::RocksDB),
            (SallyWriteBatch::TestDB(write_batch), SallyColumn::TestDB((test_db, _))) => {
                write_batch
                    .delete_range(test_db, from, to)
                    .map(SallyWriteBatch::TestDB)
            }
            _ => unimplemented!(),
        }
    }
    /// inserts a range of (key, value) pairs given as an iterator
    pub fn insert_batch<J: Borrow<K>, K: Serialize, U: Borrow<V>, V: Serialize>(
        self,
        db: &SallyColumn<K, V>,
        new_vals: impl IntoIterator<Item = (J, U)>,
    ) -> Result<Self, TypedStoreError> {
        match (self, db) {
            (SallyWriteBatch::RocksDB(db_batch), SallyColumn::RocksDB((db_map, _))) => db_batch
                .insert_batch(db_map, new_vals)
                .map(SallyWriteBatch::RocksDB),
            (SallyWriteBatch::TestDB(write_batch), SallyColumn::TestDB((test_db, _))) => {
                write_batch
                    .insert_batch(test_db, new_vals)
                    .map(SallyWriteBatch::TestDB)
            }
            _ => unimplemented!(),
        }
    }
}

/// A SallyIter provides an iterator over all key values in a sally column
pub enum SallyIter<'a, K, V> {
    // Iter for a rocksdb backed sally column when `fallback_to_db` is true
    RocksDB(RocksDBIter<'a, K, V>),
    TestDB(TestDBIter<'a, K, V>),
}

impl<'a, K: DeserializeOwned, V: DeserializeOwned> Iterator for SallyIter<'a, K, V> {
    type Item = (K, V);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            SallyIter::RocksDB(iter) => iter.next(),
            SallyIter::TestDB(iter) => iter.next(),
        }
    }
}

/// A SallyIter provides an iterator over all keys in a sally column
pub enum SallyKeys<'a, K> {
    // Iter for a rocksdb backed sally column when `fallback_to_db` is true
    RocksDB(Keys<'a, K>),
    TestDB(TestDBKeys<'a, K>),
}

impl<'a, K: DeserializeOwned> Iterator for SallyKeys<'a, K> {
    type Item = K;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            SallyKeys::RocksDB(keys) => keys.next(),
            SallyKeys::TestDB(iter) => iter.next(),
        }
    }
}

/// A SallyIter provides an iterator over all values in a sally column
pub enum SallyValues<'a, V> {
    // Iter for a rocksdb backed sally column when `fallback_to_db` is true
    RocksDB(Values<'a, V>),
    TestDB(TestDBValues<'a, V>),
}

impl<'a, V: DeserializeOwned> Iterator for SallyValues<'a, V> {
    type Item = V;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            SallyValues::RocksDB(values) => values.next(),
            SallyValues::TestDB(iter) => iter.next(),
        }
    }
}

/// Options to configure a sally db instance at the global level
pub enum SallyDBOptions {
    // Options when sally db instance is backed by a single rocksdb instance
    RocksDB(
        (
            PathBuf,
            RocksDBAccessType,
            Option<Options>,
            Option<DBMapTableConfigMap>,
        ),
    ),
    TestDB,
}

/// Options to configure a sally db instance for performing read only operations at the global level
pub enum SallyReadOnlyDBOptions {
    // Options when sally db instance is backed by a single rocksdb instance
    RocksDB((PathBuf, Option<PathBuf>, Option<Options>)),
    TestDB,
}

/// Options to configure an individual column in a sally db instance
#[derive(Clone)]
pub enum SallyColumnOptions {
    // Options to configure a rocksdb column family backed sally column
    RocksDB(DBOptions),
    TestDB,
}

impl SallyColumnOptions {
    pub fn get_rocksdb_options(&self) -> Option<&DBOptions> {
        match self {
            SallyColumnOptions::RocksDB(db_options) => Some(db_options),
            _ => None,
        }
    }
}

/// Creates a default RocksDB option, to be used when RocksDB option is not specified..
pub fn default_column_options() -> SallyColumnOptions {
    SallyColumnOptions::RocksDB(default_db_options())
}

#[derive(Clone)]
pub struct SallyDBConfigMap(BTreeMap<String, SallyColumnOptions>);
impl SallyDBConfigMap {
    pub fn new(map: BTreeMap<String, SallyColumnOptions>) -> Self {
        Self(map)
    }

    pub fn to_map(&self) -> BTreeMap<String, SallyColumnOptions> {
        self.0.clone()
    }
}
