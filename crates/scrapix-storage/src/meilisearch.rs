//! Meilisearch storage backend
//!
//! Primary storage for documents with full-text search, metadata, and vector capabilities.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use async_trait::async_trait;
use meilisearch_sdk::{
    client::{Client, SwapIndexes},
    documents::DocumentDeletionQuery,
    indexes::Index,
    settings::{PaginationSetting, Settings},
    task_info::TaskInfo,
    tasks::TasksSearchQuery,
};
use tracing::{debug, error, info, instrument, warn};

use scrapix_core::config::{FeaturesConfig, MeilisearchSettings};
use scrapix_core::{Ack, Document, JobSpec, Result, ScrapixError};

/// Meilisearch configuration
#[derive(Debug, Clone)]
pub struct MeilisearchConfig {
    /// Meilisearch URL
    pub url: String,
    /// API key
    pub api_key: Option<String>,
    /// Index UID
    pub index_uid: String,
    /// Primary key field
    pub primary_key: String,
    /// Searchable attributes
    pub searchable_attributes: Vec<String>,
    /// Filterable attributes
    pub filterable_attributes: Vec<String>,
    /// Sortable attributes
    pub sortable_attributes: Vec<String>,
    /// Displayed attributes (None = all)
    pub displayed_attributes: Option<Vec<String>>,
    /// Ranking rules
    pub ranking_rules: Option<Vec<String>>,
    /// Distinct attribute
    pub distinct_attribute: Option<String>,
    /// Max total hits for pagination
    pub max_total_hits: usize,
    /// Batch size for document indexing
    pub batch_size: usize,
    /// Timeout for operations
    pub timeout: Duration,
}

impl Default for MeilisearchConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:7700".to_string(),
            api_key: None,
            index_uid: "documents".to_string(),
            primary_key: "uid".to_string(),
            searchable_attributes: vec![
                "title".to_string(),
                "content".to_string(),
                "h1".to_string(),
                "h2".to_string(),
                "h3".to_string(),
            ],
            filterable_attributes: vec![
                "domain".to_string(),
                "source".to_string(),
                "urls_tags".to_string(),
                "language".to_string(),
                "crawled_at".to_string(),
                "_crawl_job_id".to_string(),
            ],
            sortable_attributes: vec!["crawled_at".to_string()],
            displayed_attributes: None,
            ranking_rules: None,
            distinct_attribute: None,
            max_total_hits: 10000,
            batch_size: 1000,
            timeout: Duration::from_secs(60),
        }
    }
}

/// Buffered documents waiting to be sent, each with the [`Ack`] of the
/// message it came from.
type PendingBatch = VecDeque<(Document, Ack)>;

/// Meilisearch storage backend
///
/// Documents are buffered per target index together with an [`Ack`]. A
/// flush sends each index's buffer in batches of `batch_size`; the acks of a
/// batch fire only once Meilisearch accepted it (HTTP 202 + task uid). A
/// rejected batch keeps its (document, ack) pairs, in order, for the next
/// flush. Waiting for the Meilisearch task itself to succeed is out of scope
/// (the API checks failed tasks at job completion).
pub struct MeilisearchStorage {
    client: Client,
    config: MeilisearchConfig,
    index: Index,
    pending: parking_lot::Mutex<HashMap<String, PendingBatch>>,
    /// Serializes flushes so a failed batch is re-queued before the next
    /// attempt reads the buffer (keeps per-index order, no double sends).
    flush_lock: tokio::sync::Mutex<()>,
}

impl MeilisearchStorage {
    /// Create a new Meilisearch storage client and initialize the default
    /// index (created if missing, base settings applied).
    pub async fn new(config: MeilisearchConfig) -> Result<Self> {
        let storage = Self::connect(config)?;
        storage.initialize_index().await?;
        Ok(storage)
    }

    /// Create a storage client without touching Meilisearch (no index
    /// creation, no settings). Index setup is left to
    /// [`configure_index`](Self::configure_index), which honors per-job
    /// `keep_settings`.
    pub fn connect(config: MeilisearchConfig) -> Result<Self> {
        let client = Client::new(&config.url, config.api_key.as_deref()).map_err(|e| {
            ScrapixError::Storage(format!("Failed to create Meilisearch client: {}", e))
        })?;
        let index = client.index(&config.index_uid);

        Ok(Self {
            client,
            config,
            index,
            pending: parking_lot::Mutex::new(HashMap::new()),
            flush_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Primary key used for index creation and document additions.
    pub fn primary_key(&self) -> &str {
        &self.config.primary_key
    }

    /// Documents per Meilisearch request.
    pub fn batch_size(&self) -> usize {
        self.config.batch_size.max(1)
    }

    /// Initialize index with configured settings
    #[instrument(skip(self))]
    async fn initialize_index(&self) -> Result<()> {
        // Create index if it doesn't exist
        let task = self
            .client
            .create_index(&self.config.index_uid, Some(&self.config.primary_key))
            .await
            .map_err(|e| ScrapixError::Storage(format!("Failed to create index: {}", e)))?;

        // Wait for index creation (might already exist, which is fine)
        let _ = self.wait_for_task(task).await;

        // Configure settings
        let mut settings = Settings::new();

        settings = settings
            .with_searchable_attributes(&self.config.searchable_attributes)
            .with_filterable_attributes(&self.config.filterable_attributes)
            .with_sortable_attributes(&self.config.sortable_attributes)
            .with_pagination(PaginationSetting {
                max_total_hits: self.config.max_total_hits,
            });

        if let Some(ref displayed) = self.config.displayed_attributes {
            settings = settings.with_displayed_attributes(displayed);
        }

        if let Some(ref ranking) = self.config.ranking_rules {
            settings = settings.with_ranking_rules(ranking);
        }

        if let Some(ref distinct) = self.config.distinct_attribute {
            settings = settings.with_distinct_attribute(Some(distinct));
        }

        let task =
            self.index.set_settings(&settings).await.map_err(|e| {
                ScrapixError::Storage(format!("Failed to set index settings: {}", e))
            })?;

        self.wait_for_task(task).await?;

        info!(
            index = %self.config.index_uid,
            "Meilisearch index initialized"
        );

        Ok(())
    }

    /// Add a single document to the default index (no ack tracking)
    pub async fn add_document(&self, doc: Document) -> Result<()> {
        let index_uid = self.config.index_uid.clone();
        self.add_document_to_index(doc, &index_uid, Ack::noop())
            .await
    }

    /// Add multiple documents to the default index (no ack tracking)
    pub async fn add_documents(&self, docs: Vec<Document>) -> Result<()> {
        for doc in docs {
            self.add_document(doc).await?;
        }
        Ok(())
    }

    /// Buffer `doc` for `index_uid`; `ack` fires once Meilisearch accepted
    /// the batch containing it.
    ///
    /// Reaching `batch_size` buffered documents for the index sends them
    /// right away. Backpressure: while `4 * batch_size` documents are
    /// already buffered for the index (Meilisearch keeps rejecting them),
    /// this waits and retries the flush before buffering more.
    #[instrument(skip(self, doc, ack), fields(url = %doc.url, index = %index_uid))]
    pub async fn add_document_to_index(
        &self,
        doc: Document,
        index_uid: &str,
        ack: Ack,
    ) -> Result<()> {
        let batch_size = self.batch_size();
        let mut backoff = Duration::from_millis(500);
        while self.pending_for(index_uid) >= 4 * batch_size {
            if let Err(e) = self.flush_index(index_uid).await {
                warn!(
                    index = %index_uid,
                    error = %e,
                    retry_in_ms = backoff.as_millis() as u64,
                    "Meilisearch buffer full and flush failed; waiting before retry"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }

        let buffered = {
            let mut pending = self.pending.lock();
            let queue = pending.entry(index_uid.to_string()).or_default();
            queue.push_back((doc, ack));
            queue.len()
        };

        if buffered >= batch_size {
            // A failed send keeps the documents buffered; the periodic flush
            // (or the next add) retries them.
            if let Err(e) = self.flush_index(index_uid).await {
                warn!(index = %index_uid, error = %e, "Batch flush failed; documents kept for retry");
            }
        }

        Ok(())
    }

    /// Number of buffered (not yet accepted) documents, all indexes.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().values().map(VecDeque::len).sum()
    }

    fn pending_for(&self, index_uid: &str) -> usize {
        self.pending.lock().get(index_uid).map_or(0, VecDeque::len)
    }

    /// Flush every index's buffer. Returns the number of documents accepted
    /// by Meilisearch (and acked); errors if any batch was rejected (its
    /// documents stay buffered, un-acked).
    pub async fn flush(&self) -> Result<usize> {
        let indexes: Vec<String> = self.pending.lock().keys().cloned().collect();
        let mut accepted = 0;
        let mut first_error = None;
        for index_uid in indexes {
            match self.flush_index(&index_uid).await {
                Ok(n) => accepted += n,
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(accepted),
        }
    }

    /// Send `index_uid`'s buffer in `batch_size` batches, acking each batch
    /// once accepted. Stops at the first rejected batch, putting it back at
    /// the front of the buffer.
    async fn flush_index(&self, index_uid: &str) -> Result<usize> {
        let _guard = self.flush_lock.lock().await;
        let batch_size = self.batch_size();
        let index = self.client.index(index_uid);
        let mut accepted = 0;

        loop {
            let batch: Vec<(Document, Ack)> = {
                let mut pending = self.pending.lock();
                let Some(queue) = pending.get_mut(index_uid) else {
                    break;
                };
                let n = queue.len().min(batch_size);
                let batch = queue.drain(..n).collect();
                if queue.is_empty() {
                    pending.remove(index_uid);
                }
                batch
            };
            if batch.is_empty() {
                break;
            }

            let (docs, acks): (Vec<Document>, Vec<Ack>) = batch.into_iter().unzip();
            let count = docs.len();
            debug!(count, index = %index_uid, "Submitting documents to Meilisearch");

            match index
                .add_documents(&docs, Some(&self.config.primary_key))
                .await
            {
                Ok(task) => {
                    for ack in acks {
                        ack.ack();
                    }
                    accepted += count;
                    info!(
                        count,
                        task_uid = task.task_uid,
                        index = %index_uid,
                        "Documents accepted by Meilisearch"
                    );
                }
                Err(e) => {
                    let mut pending = self.pending.lock();
                    let queue = pending.entry(index_uid.to_string()).or_default();
                    for pair in docs.into_iter().zip(acks).rev() {
                        queue.push_front(pair);
                    }
                    return Err(ScrapixError::Storage(format!(
                        "Failed to add documents to {}: {}",
                        index_uid, e
                    )));
                }
            }
        }

        Ok(accepted)
    }

    /// Wait for a Meilisearch task to complete by polling.
    ///
    /// This is public so callers can optionally wait for task completion
    /// in cases where it matters (e.g., shutdown flushes, index initialization).
    /// For normal indexing operations, tasks are submitted fire-and-forget.
    pub async fn wait_for_task(&self, task_info: TaskInfo) -> Result<()> {
        loop {
            let task =
                self.client.get_task(&task_info).await.map_err(|e| {
                    ScrapixError::Storage(format!("Failed to get task status: {}", e))
                })?;

            match task {
                meilisearch_sdk::tasks::Task::Succeeded { .. } => {
                    return Ok(());
                }
                meilisearch_sdk::tasks::Task::Failed { content } => {
                    return Err(ScrapixError::Storage(format!(
                        "Task failed: {:?}",
                        content.error
                    )));
                }
                _ => {
                    // Still processing (Enqueued or Processing)
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Prepare `index_uid` for a job: create it (with this storage's
    /// primary key) when missing, then apply settings derived from the
    /// job's features with the job's `index_settings` merged over them.
    ///
    /// With `spec.keep_settings` an already existing index is left
    /// untouched. Failures are logged, not returned: documents are still
    /// indexed with whatever settings the index has.
    pub async fn configure_index(
        &self,
        index_uid: &str,
        features: &FeaturesConfig,
        spec: Option<&JobSpec>,
    ) {
        let keep_settings = spec.is_some_and(|s| s.keep_settings);
        let exists = match self.client.get_index(index_uid).await {
            Ok(_) => true,
            Err(meilisearch_sdk::errors::Error::Meilisearch(e))
                if e.error_code == meilisearch_sdk::errors::ErrorCode::IndexNotFound =>
            {
                false
            }
            Err(e) => {
                // Unknown: don't create it, and never overwrite settings the
                // job asked to keep.
                warn!(index = %index_uid, error = %e, "Failed to look up index");
                if keep_settings {
                    return;
                }
                true
            }
        };

        if !exists {
            // Tasks on one index run in order, so settings and documents
            // submitted after this are applied to the created index.
            if let Err(e) = self
                .client
                .create_index(index_uid, Some(&self.config.primary_key))
                .await
            {
                warn!(index = %index_uid, error = %e, "Failed to create index");
            }
        } else if keep_settings {
            info!(index = %index_uid, "keep_settings: leaving existing index settings untouched");
            return;
        }

        let overrides = spec.and_then(|s| s.index_settings.as_ref());
        let settings = self.job_settings(features, overrides);
        match self.client.index(index_uid).set_settings(&settings).await {
            Ok(_) => info!(index = %index_uid, "Configured index settings for job"),
            Err(e) => warn!(
                index = %index_uid,
                error = %e,
                "Failed to configure index settings for job"
            ),
        }
    }

    /// Settings for a job: attributes dynamically derived from the enabled
    /// features, then each field the job set in `overrides` replaces the
    /// derived value. `_crawl_job_id` always stays filterable (the Replace
    /// index strategy deletes stale documents by filtering on it).
    fn job_settings(
        &self,
        features: &FeaturesConfig,
        overrides: Option<&MeilisearchSettings>,
    ) -> Settings {
        // Start from the base configured attributes
        let mut searchable = self.config.searchable_attributes.clone();
        let mut filterable = self.config.filterable_attributes.clone();
        let mut sortable = self.config.sortable_attributes.clone();

        // Markdown is an alternative text representation — searchable when enabled
        if features.markdown_enabled() {
            searchable.push("markdown".to_string());
        }

        // AI summary is free-text, good for search
        if features.ai_summary_enabled() {
            searchable.push("ai_summary".to_string());
        }

        // AI extraction is structured but may contain searchable text
        if features.ai_extraction_enabled() {
            searchable.push("ai_extraction".to_string());
        }

        // Schema.org/JSON-LD is structured data — filterable for faceting
        if features.schema_enabled() {
            filterable.push("schema".to_string());
        }

        // Custom CSS selectors produce named fields under `custom.*`
        // Each custom field should be both searchable and filterable
        if let Some(ref selectors_config) = features.custom_selectors {
            if selectors_config.enabled {
                for key in selectors_config.selectors.keys() {
                    let field = format!("custom.{}", key);
                    searchable.push(field.clone());
                    filterable.push(field);
                }
            }
        }

        // Block split: add heading sub-levels, block navigation fields, and distinct
        let mut distinct = None;
        if features.block_split_enabled() {
            searchable.extend(["h4".to_string(), "h5".to_string(), "h6".to_string()]);
            filterable.extend([
                "parent_document_id".to_string(),
                "page_block".to_string(),
                "anchor".to_string(),
            ]);
            sortable.push("page_block".to_string());
            distinct = Some("parent_document_id".to_string());
        }

        let mut settings = Settings::new();
        if let Some(o) = overrides {
            if let Some(ref v) = o.searchable_attributes {
                searchable = v.clone();
            }
            if let Some(ref v) = o.filterable_attributes {
                filterable = v.clone();
            }
            if let Some(ref v) = o.sortable_attributes {
                sortable = v.clone();
            }
            if let Some(ref v) = o.distinct_attribute {
                distinct = Some(v.clone());
            }
            if let Some(ref v) = o.ranking_rules {
                settings = settings.with_ranking_rules(v);
            }
            if let Some(ref v) = o.stop_words {
                settings = settings.with_stop_words(v);
            }
            if let Some(ref v) = o.synonyms {
                settings = settings.with_synonyms(v.clone());
            }
        }
        if !filterable.iter().any(|f| f == "_crawl_job_id") {
            filterable.push("_crawl_job_id".to_string());
        }

        settings = settings
            .with_searchable_attributes(&searchable)
            .with_filterable_attributes(&filterable)
            .with_sortable_attributes(&sortable);
        if let Some(ref d) = distinct {
            settings = settings.with_distinct_attribute(Some(d));
        }
        settings
    }

    /// Get document count in the index
    pub async fn count(&self) -> Result<u64> {
        let stats = self
            .index
            .get_stats()
            .await
            .map_err(|e| ScrapixError::Storage(format!("Failed to get stats: {}", e)))?;

        Ok(stats.number_of_documents as u64)
    }

    /// Delete a document by UID
    pub async fn delete(&self, uid: &str) -> Result<()> {
        let task = self
            .index
            .delete_document(uid)
            .await
            .map_err(|e| ScrapixError::Storage(format!("Failed to delete document: {}", e)))?;

        self.wait_for_task(task).await
    }

    /// Delete all documents
    pub async fn delete_all(&self) -> Result<()> {
        let task =
            self.index.delete_all_documents().await.map_err(|e| {
                ScrapixError::Storage(format!("Failed to delete all documents: {}", e))
            })?;

        self.wait_for_task(task).await
    }

    /// Search for documents
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Document>> {
        let results = self
            .index
            .search()
            .with_query(query)
            .with_limit(limit)
            .execute::<Document>()
            .await
            .map_err(|e| ScrapixError::Storage(format!("Search failed: {}", e)))?;

        Ok(results.hits.into_iter().map(|h| h.result).collect())
    }

    /// Get a document by UID
    pub async fn get(&self, uid: &str) -> Result<Option<Document>> {
        match self.index.get_document::<Document>(uid).await {
            Ok(doc) => Ok(Some(doc)),
            Err(meilisearch_sdk::errors::Error::Meilisearch(e))
                if e.error_code == meilisearch_sdk::errors::ErrorCode::DocumentNotFound =>
            {
                Ok(None)
            }
            Err(e) => Err(ScrapixError::Storage(format!(
                "Failed to get document: {}",
                e
            ))),
        }
    }

    /// Get index health status
    pub async fn health(&self) -> Result<bool> {
        match self.client.health().await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Perform an atomic index swap between two indexes, then delete the old one.
    ///
    /// This is a static helper that creates a throwaway Meilisearch client,
    /// waits for all pending tasks on the temp index to settle, performs the
    /// atomic swap, and deletes the temp index (which now holds old data).
    pub async fn perform_swap(
        meilisearch_url: &str,
        api_key: Option<&str>,
        target_index: &str,
        temp_index: &str,
    ) -> Result<()> {
        let client = Client::new(meilisearch_url, api_key).map_err(|e| {
            ScrapixError::Storage(format!(
                "Failed to create Meilisearch client for swap: {}",
                e
            ))
        })?;

        // Ensure the target index exists (first crawl case)
        let _ = client.create_index(target_index, Some("uid")).await;

        // Wait for all pending indexing tasks on the temp index to complete
        Self::wait_for_index_idle_with_client(&client, temp_index, Duration::from_secs(300))
            .await?;

        info!(
            target = %target_index,
            temp = %temp_index,
            "All indexing tasks settled, performing atomic swap"
        );

        // Perform the atomic swap
        let swap = SwapIndexes {
            indexes: (target_index.to_string(), temp_index.to_string()),
        };
        let task_info = client.swap_indexes([&swap]).await.map_err(|e| {
            ScrapixError::Storage(format!(
                "Failed to swap indexes {} <-> {}: {}",
                target_index, temp_index, e
            ))
        })?;

        // Wait for swap to complete
        task_info
            .wait_for_completion(
                &client,
                Some(Duration::from_millis(200)),
                Some(Duration::from_secs(60)),
            )
            .await
            .map_err(|e| ScrapixError::Storage(format!("Swap task failed: {}", e)))?;

        info!(
            target = %target_index,
            temp = %temp_index,
            "Index swap completed successfully"
        );

        // Delete the temp index (which now holds old data)
        if let Err(e) = Self::delete_index_with_client(&client, temp_index).await {
            warn!(
                temp = %temp_index,
                error = %e,
                "Failed to delete temp index after swap (non-fatal)"
            );
        }

        Ok(())
    }

    /// Delete all documents in an index that were NOT indexed by the given job.
    /// Used by the Replace index strategy: after a crawl completes, this removes
    /// stale documents from previous crawls while preserving freshly-crawled ones.
    pub async fn delete_stale_documents(
        meilisearch_url: &str,
        api_key: Option<&str>,
        index_uid: &str,
        job_id: &str,
    ) -> Result<()> {
        let client = Client::new(meilisearch_url, api_key).map_err(|e| {
            ScrapixError::Storage(format!(
                "Failed to create Meilisearch client for stale cleanup: {}",
                e,
            ))
        })?;

        // Wait for all pending indexing tasks to settle first
        Self::wait_for_index_idle_with_client(&client, index_uid, Duration::from_secs(300)).await?;

        info!(
            index = %index_uid,
            job_id = %job_id,
            "Deleting stale documents not indexed by this crawl job"
        );

        let index = client.index(index_uid);
        let filter = format!("_crawl_job_id != '{}'", job_id);
        let mut query = DocumentDeletionQuery::new(&index);
        query.with_filter(&filter);
        let task_info = index.delete_documents_with(&query).await.map_err(|e| {
            ScrapixError::Storage(format!(
                "Failed to delete stale documents from {}: {}",
                index_uid, e,
            ))
        })?;

        task_info
            .wait_for_completion(
                &client,
                Some(Duration::from_millis(200)),
                Some(Duration::from_secs(300)),
            )
            .await
            .map_err(|e| {
                ScrapixError::Storage(format!(
                    "Delete stale documents task failed for {}: {}",
                    index_uid, e,
                ))
            })?;

        info!(
            index = %index_uid,
            job_id = %job_id,
            "Stale document cleanup completed"
        );
        Ok(())
    }

    /// Delete a temp index (best-effort cleanup, e.g. on job failure).
    pub async fn cleanup_temp_index(
        meilisearch_url: &str,
        api_key: Option<&str>,
        index_uid: &str,
    ) -> Result<()> {
        let client = Client::new(meilisearch_url, api_key).map_err(|e| {
            ScrapixError::Storage(format!(
                "Failed to create Meilisearch client for cleanup: {}",
                e
            ))
        })?;

        Self::delete_index_with_client(&client, index_uid).await
    }

    /// Delete an index via the given client.
    async fn delete_index_with_client(client: &Client, index_uid: &str) -> Result<()> {
        let task_info = client.index(index_uid).delete().await.map_err(|e| {
            ScrapixError::Storage(format!("Failed to delete index {}: {}", index_uid, e))
        })?;

        task_info
            .wait_for_completion(
                client,
                Some(Duration::from_millis(200)),
                Some(Duration::from_secs(30)),
            )
            .await
            .map_err(|e| {
                ScrapixError::Storage(format!("Delete index task failed for {}: {}", index_uid, e))
            })?;

        info!(index = %index_uid, "Index deleted");
        Ok(())
    }

    /// Query the actual document count for an index directly from Meilisearch.
    /// Returns None if the index doesn't exist or the query fails.
    pub async fn get_actual_document_count(
        meilisearch_url: &str,
        api_key: Option<&str>,
        index_uid: &str,
    ) -> Option<u64> {
        let client = Client::new(meilisearch_url, api_key).ok()?;
        let stats = client.index(index_uid).get_stats().await.ok()?;
        Some(stats.number_of_documents as u64)
    }

    /// Check for recently failed Meilisearch tasks on a given index and log them.
    /// Returns the number of failed tasks found.
    pub async fn log_failed_tasks(
        meilisearch_url: &str,
        api_key: Option<&str>,
        index_uid: &str,
    ) -> u32 {
        let client = match Client::new(meilisearch_url, api_key) {
            Ok(c) => c,
            Err(_) => return 0,
        };

        let mut query = TasksSearchQuery::new(&client);
        query.with_index_uids([index_uid]).with_statuses(["failed"]);

        let result = match client.get_tasks_with(&query).await {
            Ok(r) => r,
            Err(e) => {
                warn!(index = %index_uid, error = %e, "Failed to query failed tasks");
                return 0;
            }
        };

        let count = result.results.len() as u32;
        if count > 0 {
            error!(
                index = %index_uid,
                failed_task_count = count,
                "Meilisearch indexing tasks failed — documents may not have been indexed"
            );
            for task in &result.results {
                error!(index = %index_uid, task = ?task, "Failed Meilisearch task detail");
            }
        }
        count
    }

    /// Wait until all tasks for a specific index are finished (no enqueued/processing tasks).
    async fn wait_for_index_idle_with_client(
        client: &Client,
        index_uid: &str,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(ScrapixError::Storage(format!(
                    "Timed out waiting for index {} tasks to settle ({}s)",
                    index_uid,
                    timeout.as_secs()
                )));
            }

            let mut query = TasksSearchQuery::new(client);
            query
                .with_index_uids([index_uid])
                .with_statuses(["enqueued", "processing"]);

            let result = match client.get_tasks_with(&query).await {
                Ok(r) => r,
                Err(e) => {
                    let msg = e.to_string();
                    // If the key lacks tasks.get permission, we can't poll — but Meilisearch
                    // processes tasks sequentially per index, so the delete filter submitted
                    // next will naturally queue after any pending indexing tasks.
                    if msg.contains("invalid_api_key")
                        || msg.contains("missing_authorization_header")
                        || msg.contains("auth")
                    {
                        warn!(
                            index = %index_uid,
                            "API key lacks task-query permission; skipping idle wait and relying on Meilisearch task ordering"
                        );
                        return Ok(());
                    }
                    return Err(ScrapixError::Storage(format!(
                        "Failed to query tasks for index {}: {}",
                        index_uid, e
                    )));
                }
            };

            let pending_count = result.results.len();
            if pending_count == 0 {
                return Ok(());
            }

            debug!(
                index = %index_uid,
                pending = pending_count,
                "Waiting for index tasks to settle"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Implementation of core Storage trait
#[async_trait]
impl scrapix_core::traits::Storage for MeilisearchStorage {
    async fn add(&self, doc: Document) -> Result<()> {
        self.add_document(doc).await
    }

    async fn add_batch(&self, docs: Vec<Document>) -> Result<()> {
        self.add_documents(docs).await
    }

    async fn flush(&self) -> Result<usize> {
        MeilisearchStorage::flush(self).await
    }

    async fn count(&self) -> Result<u64> {
        MeilisearchStorage::count(self).await
    }
}

/// Builder for MeilisearchStorage
pub struct MeilisearchStorageBuilder {
    config: MeilisearchConfig,
}

impl MeilisearchStorageBuilder {
    pub fn new(url: impl Into<String>, index_uid: impl Into<String>) -> Self {
        Self {
            config: MeilisearchConfig {
                url: url.into(),
                index_uid: index_uid.into(),
                ..Default::default()
            },
        }
    }

    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.config.api_key = Some(key.into());
        self
    }

    pub fn primary_key(mut self, key: impl Into<String>) -> Self {
        self.config.primary_key = key.into();
        self
    }

    pub fn searchable_attributes(mut self, attrs: Vec<String>) -> Self {
        self.config.searchable_attributes = attrs;
        self
    }

    pub fn filterable_attributes(mut self, attrs: Vec<String>) -> Self {
        self.config.filterable_attributes = attrs;
        self
    }

    pub fn sortable_attributes(mut self, attrs: Vec<String>) -> Self {
        self.config.sortable_attributes = attrs;
        self
    }

    pub fn distinct_attribute(mut self, attr: impl Into<String>) -> Self {
        self.config.distinct_attribute = Some(attr.into());
        self
    }

    pub fn batch_size(mut self, size: usize) -> Self {
        self.config.batch_size = size;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    pub async fn build(self) -> Result<MeilisearchStorage> {
        MeilisearchStorage::new(self.config).await
    }

    /// Build without touching Meilisearch (see [`MeilisearchStorage::connect`]).
    pub fn connect(self) -> Result<MeilisearchStorage> {
        MeilisearchStorage::connect(self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = MeilisearchConfig::default();
        assert_eq!(config.url, "http://localhost:7700");
        assert_eq!(config.index_uid, "documents");
        assert_eq!(config.primary_key, "uid");
        assert_eq!(config.batch_size, 1000);
    }

    #[test]
    fn test_builder() {
        let builder = MeilisearchStorageBuilder::new("http://localhost:7700", "test_index")
            .api_key("my_key")
            .batch_size(500);

        assert_eq!(builder.config.url, "http://localhost:7700");
        assert_eq!(builder.config.index_uid, "test_index");
        assert_eq!(builder.config.api_key, Some("my_key".to_string()));
        assert_eq!(builder.config.batch_size, 500);
    }
}
