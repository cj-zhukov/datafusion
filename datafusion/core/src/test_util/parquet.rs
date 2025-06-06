// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Helpers for writing parquet files and reading them back

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use crate::arrow::{datatypes::SchemaRef, record_batch::RecordBatch};
use crate::common::ToDFSchema;
use crate::config::ConfigOptions;
use crate::datasource::listing::{ListingTableUrl, PartitionedFile};
use crate::datasource::object_store::ObjectStoreUrl;
use crate::datasource::physical_plan::ParquetSource;
use crate::error::Result;
use crate::logical_expr::execution_props::ExecutionProps;
use crate::logical_expr::simplify::SimplifyContext;
use crate::optimizer::simplify_expressions::ExprSimplifier;
use crate::physical_expr::create_physical_expr;
use crate::physical_plan::filter::FilterExec;
use crate::physical_plan::metrics::MetricsSet;
use crate::physical_plan::ExecutionPlan;
use crate::prelude::{Expr, SessionConfig, SessionContext};

use datafusion_datasource::file::FileSource;
use datafusion_datasource::file_scan_config::FileScanConfigBuilder;
use datafusion_datasource::source::DataSourceExec;
use object_store::path::Path;
use object_store::ObjectMeta;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

///  a ParquetFile that has been created for testing.
pub struct TestParquetFile {
    path: PathBuf,
    schema: SchemaRef,
    object_store_url: ObjectStoreUrl,
    object_meta: ObjectMeta,
}

#[derive(Debug, Clone, Copy)]
/// Options for how to create the parquet scan
pub struct ParquetScanOptions {
    /// Enable pushdown filters
    pub pushdown_filters: bool,
    /// enable reordering filters
    pub reorder_filters: bool,
    /// enable page index
    pub enable_page_index: bool,
}

impl ParquetScanOptions {
    /// Returns a [`SessionConfig`] with the given options
    pub fn config(&self) -> SessionConfig {
        let mut config = ConfigOptions::new();
        config.execution.parquet.pushdown_filters = self.pushdown_filters;
        config.execution.parquet.reorder_filters = self.reorder_filters;
        config.execution.parquet.enable_page_index = self.enable_page_index;
        config.into()
    }
}

impl TestParquetFile {
    /// Creates a new parquet file at the specified location with the
    /// given properties
    pub fn try_new(
        path: PathBuf,
        props: WriterProperties,
        batches: impl IntoIterator<Item = RecordBatch>,
    ) -> Result<Self> {
        let file = File::create(&path)?;

        let mut batches = batches.into_iter();
        let first_batch = batches.next().expect("need at least one record batch");
        let schema = first_batch.schema();

        let mut writer = ArrowWriter::try_new(file, Arc::clone(&schema), Some(props))?;

        writer.write(&first_batch)?;
        let mut num_rows = first_batch.num_rows();

        for batch in batches {
            writer.write(&batch)?;
            num_rows += batch.num_rows();
        }
        writer.close()?;

        println!("Generated test dataset with {num_rows} rows");

        let size = std::fs::metadata(&path)?.len();

        let mut canonical_path = path.canonicalize()?;

        if cfg!(target_os = "windows") {
            canonical_path = canonical_path
                .to_str()
                .unwrap()
                .replace("\\", "/")
                .strip_prefix("//?/")
                .unwrap()
                .into();
        };

        let object_store_url =
            ListingTableUrl::parse(canonical_path.to_str().unwrap_or_default())?
                .object_store();

        let object_meta = ObjectMeta {
            location: Path::parse(canonical_path.to_str().unwrap_or_default())?,
            last_modified: Default::default(),
            size,
            e_tag: None,
            version: None,
        };

        Ok(Self {
            path,
            schema,
            object_store_url,
            object_meta,
        })
    }
}

impl TestParquetFile {
    /// Return a `DataSourceExec` with the specified options.
    ///
    /// If `maybe_filter` is non-None, the DataSourceExec will be filtered using
    /// the given expression, and this method will return the same plan that DataFusion
    /// will make with a pushed down predicate followed by a filter:
    ///
    /// ```text
    /// (FilterExec)
    ///   (DataSourceExec)
    /// ```
    ///
    /// Otherwise if `maybe_filter` is None, return just a `DataSourceExec`
    pub async fn create_scan(
        &self,
        ctx: &SessionContext,
        maybe_filter: Option<Expr>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let parquet_options = ctx.copied_table_options().parquet;
        let source = Arc::new(ParquetSource::new(parquet_options.clone()));
        let scan_config_builder = FileScanConfigBuilder::new(
            self.object_store_url.clone(),
            Arc::clone(&self.schema),
            source,
        )
        .with_file(PartitionedFile {
            object_meta: self.object_meta.clone(),
            partition_values: vec![],
            range: None,
            statistics: None,
            extensions: None,
            metadata_size_hint: None,
        });

        let df_schema = Arc::clone(&self.schema).to_dfschema_ref()?;

        // run coercion on the filters to coerce types etc.
        let props = ExecutionProps::new();
        let context = SimplifyContext::new(&props).with_schema(Arc::clone(&df_schema));
        if let Some(filter) = maybe_filter {
            let simplifier = ExprSimplifier::new(context);
            let filter = simplifier.coerce(filter, &df_schema).unwrap();
            let physical_filter_expr =
                create_physical_expr(&filter, &df_schema, &ExecutionProps::default())?;

            let source = Arc::new(
                ParquetSource::new(parquet_options)
                    .with_predicate(Arc::clone(&physical_filter_expr)),
            )
            .with_schema(Arc::clone(&self.schema));
            let config = scan_config_builder.with_source(source).build();
            let parquet_exec = DataSourceExec::from_data_source(config);

            let exec = Arc::new(FilterExec::try_new(physical_filter_expr, parquet_exec)?);
            Ok(exec)
        } else {
            let config = scan_config_builder.build();
            Ok(DataSourceExec::from_data_source(config))
        }
    }

    /// Retrieve metrics from the parquet exec returned from `create_scan`
    ///
    /// Recursively searches for DataSourceExec and returns the metrics
    /// on the first one it finds
    pub fn parquet_metrics(plan: &Arc<dyn ExecutionPlan>) -> Option<MetricsSet> {
        if let Some(data_source_exec) = plan.as_any().downcast_ref::<DataSourceExec>() {
            if data_source_exec
                .downcast_to_file_source::<ParquetSource>()
                .is_some()
            {
                return data_source_exec.metrics();
            }
        }

        for child in plan.children() {
            if let Some(metrics) = Self::parquet_metrics(child) {
                return Some(metrics);
            }
        }
        None
    }

    /// The schema of this parquet file
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// The path to the parquet file
    pub fn path(&self) -> &std::path::Path {
        self.path.as_path()
    }
}
