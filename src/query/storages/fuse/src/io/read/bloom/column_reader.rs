// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use std::sync::Arc;

use common_arrow::arrow::datatypes::DataType;
use common_arrow::arrow::datatypes::Field as ArrowField;
use common_arrow::arrow::io::parquet::read::column_iter_to_arrays;
use common_arrow::parquet::compression::Compression;
use common_arrow::parquet::metadata::ColumnChunkMetaData;
use common_arrow::parquet::metadata::ColumnDescriptor;
use common_arrow::parquet::read::BasicDecompressor;
use common_arrow::parquet::read::PageMetaData;
use common_arrow::parquet::read::PageReader;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::Column;
use opendal::Operator;
use storages_common_cache::CacheKey;
use storages_common_cache::InMemoryItemCacheReader;
use storages_common_cache::LoadParams;
use storages_common_cache::LoaderWithCacheKey;
use storages_common_index::filters::Filter;
use storages_common_index::filters::Xor8Filter;
use storages_common_table_meta::caches::CacheManager;
use storages_common_table_meta::meta::ColumnId;
use xorfilter::Xor8;

type CachedReader = InMemoryItemCacheReader<Xor8, Xor8Loader>;

/// An wrapper of [InMemoryBytesCacheReader], uses [ColumnDataLoader] to
/// load the data of a given bloom index column. Also
/// - takes cares of getting the correct cache instance from [CacheManager]
/// - generates the proper cache key
///
/// this could be generified to be the template of cached data block column reader as well
pub struct BloomIndexColumnReader {
    cached_reader: CachedReader,
    param: LoadParams,
}

impl BloomIndexColumnReader {
    pub fn new(
        index_path: String,
        column_id: ColumnId,
        colum_chunk_meta: &ColumnChunkMetaData,
        operator: Operator,
    ) -> Self {
        let meta = colum_chunk_meta.metadata();
        let cache_key = format!("{index_path}-{column_id}");
        let loader = Xor8Loader {
            offset: meta.data_page_offset as u64,
            len: meta.total_compressed_size as u64,
            cache_key,
            operator,
            column_descriptor: colum_chunk_meta.descriptor().clone(),
        };

        let cached_reader = CachedReader::new(
            CacheManager::instance().get_bloom_index_cache(),
            "bloom_index_data_cache".to_owned(),
            loader,
        );

        let param = LoadParams {
            location: index_path,
            len_hint: None,
            ver: 0,
        };

        BloomIndexColumnReader {
            cached_reader,
            param,
        }
    }

    pub async fn read(&self) -> Result<Arc<Xor8>> {
        self.cached_reader.read(&self.param).await
    }
}

/// Loader that fetch range of the target object with customized cache key
pub struct Xor8Loader {
    pub offset: u64,
    pub len: u64,
    pub cache_key: String,
    pub operator: Operator,
    pub column_descriptor: ColumnDescriptor,
}

#[async_trait::async_trait]
impl LoaderWithCacheKey<Xor8> for Xor8Loader {
    async fn load_with_cache_key(&self, params: &LoadParams) -> Result<Xor8> {
        let reader = self.operator.object(&params.location);
        let bytes = reader
            .range_read(self.offset..self.offset + self.len)
            .await?;

        let page_meta_data = PageMetaData {
            column_start: 0,
            num_values: 1,
            compression: Compression::Uncompressed,
            descriptor: self.column_descriptor.descriptor.clone(),
        };

        let page_reader = PageReader::new_with_page_meta(
            std::io::Cursor::new(bytes), /* we can not use &[u8] as Reader here, lifetime not valid */
            page_meta_data,
            Arc::new(|_, _| true),
            vec![],
            usize::MAX,
        );

        let decompressor = BasicDecompressor::new(page_reader, vec![]);
        let column_type = self.column_descriptor.descriptor.primitive_type.clone();
        let filed_name = self.column_descriptor.path_in_schema[0].to_owned();
        let field = ArrowField::new(filed_name, DataType::Binary, false);
        let mut array_iter =
            column_iter_to_arrays(vec![decompressor], vec![&column_type], field, None, 1)?;
        if let Some(array) = array_iter.next() {
            let array = array?;
            let col =
                Column::from_arrow(array.as_ref(), &common_expression::types::DataType::String);
            let bytes = unsafe { col.as_string().unwrap().index_unchecked(0) };
            let (filter, _size) = Xor8Filter::from_bytes(bytes)?;
            Ok(filter.filter)
        } else {
            Err(ErrorCode::StorageOther(
                "bloom index data not available as expected",
            ))
        }
    }

    fn cache_key(&self, _params: &LoadParams) -> CacheKey {
        self.cache_key.clone()
    }
}
