// Copyright 2021 Datafuse Labs
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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use common_arrow::arrow::chunk::Chunk;
use common_arrow::arrow::datatypes::Field;
use common_arrow::arrow::io::parquet::read::column_iter_to_arrays;
use common_arrow::arrow::io::parquet::read::ArrayIter;
use common_arrow::parquet::compression::Compression as ParquetCompression;
use common_arrow::parquet::metadata::ColumnDescriptor;
use common_arrow::parquet::read::PageMetaData;
use common_arrow::parquet::read::PageReader;
use common_catalog::plan::PartInfoPtr;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::ColumnId;
use common_expression::DataBlock;
use common_storage::ColumnNode;
use storages_common_cache::CacheAccessor;
use storages_common_cache::TableDataCacheKey;
use storages_common_cache_manager::CacheManager;
use storages_common_table_meta::meta::ColumnMeta;
use storages_common_table_meta::meta::Compression;

use super::block_reader_deserialize::DeserializedArray;
use super::block_reader_deserialize::FieldDeserializationContext;
use crate::fuse_part::FusePartInfo;
use crate::io::read::block::block_reader_merge_io::DataItem;
use crate::io::read::block::decompressor::BuffedBasicDecompressor;
use crate::io::BlockReader;
use crate::io::UncompressedBuffer;
use crate::metrics::*;

impl BlockReader {
    /// Deserialize column chunks data from parquet format to DataBlock.
    pub(super) fn deserialize_parquet_chunks(
        &self,
        part: PartInfoPtr,
        chunks: HashMap<ColumnId, DataItem>,
    ) -> Result<DataBlock> {
        let part = FusePartInfo::from_part(&part)?;
        let start = Instant::now();

        if chunks.is_empty() {
            return self.build_default_values_block(part.nums_rows);
        }

        let deserialized_res = self.deserialize_parquet_chunks_with_buffer(
            &part.location,
            part.nums_rows,
            &part.compression,
            &part.columns_meta,
            chunks,
            None,
        );

        // Perf.
        {
            metrics_inc_remote_io_deserialize_milliseconds(start.elapsed().as_millis() as u64);
        }

        deserialized_res
    }

    pub fn build_default_values_block(&self, num_rows: usize) -> Result<DataBlock> {
        let data_schema = self.data_schema();
        let default_vals = self.default_vals.clone();
        DataBlock::create_with_default_value(&data_schema, &default_vals, num_rows)
    }

    /// Deserialize column chunks data from parquet format to DataBlock with a uncompressed buffer.
    pub(crate) fn deserialize_parquet_chunks_with_buffer(
        &self,
        block_path: &str,
        num_rows: usize,
        compression: &Compression,
        column_metas: &HashMap<ColumnId, ColumnMeta>,
        column_chunks: HashMap<ColumnId, DataItem>,
        uncompressed_buffer: Option<Arc<UncompressedBuffer>>,
    ) -> Result<DataBlock> {
        if column_chunks.is_empty() {
            return self.build_default_values_block(num_rows);
        }

        let mut need_default_vals = Vec::with_capacity(self.project_column_nodes.len());
        let mut need_to_fill_default_val = false;
        let mut deserialized_column_arrays = Vec::with_capacity(self.projection.len());
        let field_deserialization_ctx = FieldDeserializationContext {
            column_metas,
            column_chunks: &column_chunks,
            num_rows,
            compression,
            uncompressed_buffer: &uncompressed_buffer,
        };
        for column_node in &self.project_column_nodes {
            match self.deserialize_field(&field_deserialization_ctx, column_node)? {
                None => {
                    need_to_fill_default_val = true;
                    need_default_vals.push(true);
                }
                Some(v) => {
                    deserialized_column_arrays.push(v);
                    need_default_vals.push(false);
                }
            }
        }

        // assembly the arrays
        let mut chunk_arrays = vec![];
        for array in &deserialized_column_arrays {
            match array {
                DeserializedArray::Deserialized((_, array, ..)) => {
                    chunk_arrays.push(array);
                }
                DeserializedArray::NoNeedToCache(array) => {
                    chunk_arrays.push(array);
                }
                DeserializedArray::Cached(sized_column) => {
                    chunk_arrays.push(&sized_column.0);
                }
            }
        }

        // build data block
        let chunk = Chunk::try_new(chunk_arrays)?;
        let data_block = if !need_to_fill_default_val {
            DataBlock::from_arrow_chunk(&chunk, &self.data_schema())?
        } else {
            let data_schema = self.data_schema();
            let mut default_vals = Vec::with_capacity(need_default_vals.len());
            for (i, need_default_val) in need_default_vals.iter().enumerate() {
                if !need_default_val {
                    default_vals.push(None);
                } else {
                    default_vals.push(Some(self.default_vals[i].clone()));
                }
            }
            DataBlock::create_with_default_value_and_chunk(
                &data_schema,
                &chunk,
                &default_vals,
                num_rows,
            )?
        };

        // populate cache if necessary
        if let Some(cache) = CacheManager::instance().get_table_data_array_cache() {
            // populate array cache items
            for item in deserialized_column_arrays.into_iter() {
                if let DeserializedArray::Deserialized((column_id, array, size)) = item {
                    let key = TableDataCacheKey::new(block_path, column_id);
                    cache.put(key.into(), Arc::new((array, size)))
                }
            }
        }
        Ok(data_block)
    }

    fn chunks_to_parquet_array_iter<'a>(
        metas: Vec<&ColumnMeta>,
        chunks: Vec<&'a [u8]>,
        rows: usize,
        column_descriptors: Vec<&ColumnDescriptor>,
        field: Field,
        compression: &Compression,
        uncompressed_buffer: Arc<UncompressedBuffer>,
    ) -> Result<ArrayIter<'a>> {
        let columns = metas
            .iter()
            .zip(chunks.into_iter().zip(column_descriptors.iter()))
            .map(|(meta, (chunk, column_descriptor))| {
                let meta = meta.as_parquet().unwrap();

                let page_meta_data = PageMetaData {
                    column_start: meta.offset,
                    num_values: meta.num_values as i64,
                    compression: Self::to_parquet_compression(compression)?,
                    descriptor: column_descriptor.descriptor.clone(),
                };
                let pages = PageReader::new_with_page_meta(
                    chunk,
                    page_meta_data,
                    Arc::new(|_, _| true),
                    vec![],
                    usize::MAX,
                );

                Ok(BuffedBasicDecompressor::new(
                    pages,
                    uncompressed_buffer.clone(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        let types = column_descriptors
            .iter()
            .map(|column_descriptor| &column_descriptor.descriptor.primitive_type)
            .collect::<Vec<_>>();

        Ok(column_iter_to_arrays(
            columns,
            types,
            field,
            Some(rows),
            rows,
        )?)
    }

    fn deserialize_field<'a>(
        &self,
        deserialization_context: &'a FieldDeserializationContext,
        column: &ColumnNode,
    ) -> Result<Option<DeserializedArray<'a>>> {
        let indices = &column.leaf_indices;
        let column_chunks = deserialization_context.column_chunks;
        let compression = deserialization_context.compression;
        let uncompressed_buffer = deserialization_context.uncompressed_buffer;
        // column passed in may be a compound field (with sub leaves),
        // or a leaf column of compound field
        let is_nested = column.has_children();
        let estimated_cap = indices.len();
        let mut field_column_metas = Vec::with_capacity(estimated_cap);
        let mut field_column_data = Vec::with_capacity(estimated_cap);
        let mut field_column_descriptors = Vec::with_capacity(estimated_cap);
        let mut field_uncompressed_size = 0;

        for (i, leaf_index) in indices.iter().enumerate() {
            let column_id = column.leaf_column_ids[i];
            if let Some(column_meta) = deserialization_context.column_metas.get(&column_id) {
                if let Some(chunk) = column_chunks.get(&column_id) {
                    match chunk {
                        DataItem::RawData(data) => {
                            let column_descriptor =
                                &self.parquet_schema_descriptor.columns()[*leaf_index];
                            field_column_metas.push(column_meta);
                            field_column_data.push(*data);
                            field_column_descriptors.push(column_descriptor);
                            field_uncompressed_size += data.len();
                        }
                        DataItem::ColumnArray(column_array) => {
                            if is_nested {
                                // TODO more context info for error message
                                return Err(ErrorCode::StorageOther(
                                    "unexpected nested field: nested leaf field hits cached",
                                ));
                            }
                            // since it is not nested, one column is enough
                            return Ok(Some(DeserializedArray::Cached(column_array)));
                        }
                    }
                } else {
                    // TODO more context info for error message
                    // no raw data of given column id, it is unexpected
                    return Err(ErrorCode::StorageOther("unexpected: column data not found"));
                }
            } else {
                // no column meta of given column id
                break;
            }
        }

        let num_rows = deserialization_context.num_rows;
        if !field_column_metas.is_empty() {
            let field_name = column.field.name.to_owned();
            let mut array_iter = Self::chunks_to_parquet_array_iter(
                field_column_metas,
                field_column_data,
                num_rows,
                field_column_descriptors,
                column.field.clone(),
                compression,
                uncompressed_buffer
                    .clone()
                    .unwrap_or_else(|| UncompressedBuffer::new(0)),
            )?;
            let array = array_iter.next().transpose()?.ok_or_else(|| {
                ErrorCode::StorageOther(format!(
                    "unexpected deserialization error, no array found for field {field_name} "
                ))
            })?;

            // mark the array
            if is_nested {
                // the array is not intended to be cached
                // currently, caching of compound filed columns is not support
                Ok(Some(DeserializedArray::NoNeedToCache(array)))
            } else {
                // the array is deserialized from raw bytes, should be cached
                let column_id = column.leaf_column_ids[0];
                Ok(Some(DeserializedArray::Deserialized((
                    column_id,
                    array,
                    field_uncompressed_size,
                ))))
            }
        } else {
            Ok(None)
        }
    }

    fn to_parquet_compression(meta_compression: &Compression) -> Result<ParquetCompression> {
        match meta_compression {
            Compression::Lz4 => {
                let err_msg = r#"Deprecated compression algorithm [Lz4] detected.

                                        The Legacy compression algorithm [Lz4] is no longer supported.
                                        To migrate data from old format, please consider re-create the table,
                                        by using an old compatible version [v0.8.25-nightly … v0.7.12-nightly].

                                        - Bring up the compatible version of databend-query
                                        - re-create the table
                                           Suppose the name of table is T
                                            ~~~
                                            create table tmp_t as select * from T;
                                            drop table T all;
                                            alter table tmp_t rename to T;
                                            ~~~
                                        Please note that the history of table T WILL BE LOST.
                                       "#;
                Err(ErrorCode::StorageOther(err_msg))
            }
            Compression::Lz4Raw => Ok(ParquetCompression::Lz4Raw),
            Compression::Snappy => Ok(ParquetCompression::Snappy),
            Compression::Zstd => Ok(ParquetCompression::Zstd),
            Compression::Gzip => Ok(ParquetCompression::Gzip),
            Compression::None => Ok(ParquetCompression::Uncompressed),
        }
    }
}
