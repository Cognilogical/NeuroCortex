use crate::embedded_llm::evaluate_intent_embedded;
use crate::models::{BehavioralRule, ValidateVerdict};
use arrow::array::{
    FixedSizeListArray, Float32Array, RecordBatch, RecordBatchReader, StringArray, UInt32Array,
};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatchIterator;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use futures::StreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use log::{error, info};
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone)]
pub struct AcceptableEmbedder {
    pub model_name: String,
    pub dimensions: usize,
}

fn get_acceptable_embedders() -> anyhow::Result<Vec<AcceptableEmbedder>> {
    let config_dir = match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".config/neurostrata"),
        Err(_) => PathBuf::from(".neurostrata"),
    };

    if !config_dir.exists() {
        fs::create_dir_all(&config_dir)?;
    }

    let config_path = config_dir.join("embedders.json");

    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        if let Ok(models) = serde_json::from_str::<Vec<AcceptableEmbedder>>(&content) {
            if !models.is_empty() {
                return Ok(models);
            }
        }
    }

    // Default list if file doesn't exist or is empty
    let default_models = vec![
        AcceptableEmbedder {
            model_name: "NomicEmbedTextV15".to_string(),
            dimensions: 768,
        },
        AcceptableEmbedder {
            model_name: "BGEBaseENV15".to_string(),
            dimensions: 768,
        },
    ];

    if let Ok(json_content) = serde_json::to_string_pretty(&default_models) {
        let _ = fs::write(&config_path, json_content);
    }

    Ok(default_models)
}

fn find_existing_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = PathBuf::from(home);
    let primary_neuro_cache = home_path.join(".cache/neuro/models/fastembed");

    let cache_dirs = vec![
        primary_neuro_cache.clone(),
        home_path.join(".cache/fastembed"),
        home_path.join(".cache/huggingface/hub"),
    ];

    for dir in cache_dirs {
        if dir.exists()
            && dir
                .read_dir()
                .map(|mut i| i.next().is_some())
                .unwrap_or(false)
        {
            return dir;
        }
    }

    primary_neuro_cache
}

pub struct SemanticEvaluator {
    db_uri: String,
    embedder: TextEmbedding,
    dimensions: usize,
}

impl SemanticEvaluator {
    pub async fn new(db_uri: String) -> anyhow::Result<Self> {
        let acceptable_models = get_acceptable_embedders()?;
        let target_model = &acceptable_models[0];

        let model_enum = EmbeddingModel::from_str(&target_model.model_name)
            .unwrap_or(EmbeddingModel::NomicEmbedTextV15);

        let cache_dir = find_existing_cache_dir();

        if !cache_dir.exists() {
            fs::create_dir_all(&cache_dir)?;
        }

        info!(
            "Initializing SemanticEvaluator with model: {} using cache: {:?}",
            target_model.model_name, cache_dir
        );

        let embedder = TextEmbedding::try_new(
            InitOptions::new(model_enum)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(true),
        )
        .map_err(|e| anyhow::anyhow!("Failed to initialize embedder: {}", e))?;

        Ok(Self {
            db_uri,
            embedder,
            dimensions: target_model.dimensions,
        })
    }

    fn schema(&self) -> Arc<ArrowSchema> {
        Arc::new(ArrowSchema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    self.dimensions as i32,
                ),
                false,
            ),
            Field::new("version", DataType::UInt32, false),
            Field::new("rule_class", DataType::Utf8, false),
            Field::new("trigger_pattern", DataType::Utf8, false),
            Field::new("constraint_text", DataType::Utf8, false),
            Field::new("hit_count", DataType::UInt32, false),
            Field::new("status", DataType::Utf8, false),
        ]))
    }

    pub async fn add_constraint(&self, rule: BehavioralRule) -> anyhow::Result<()> {
        let conn = lancedb::connect(&self.db_uri).execute().await?;
        let schema = self.schema();

        // 1. Create table if not exists
        if !conn
            .table_names()
            .execute()
            .await?
            .contains(&"behavioral_rules".to_string())
        {
            let empty_batch = RecordBatch::new_empty(schema.clone());
            let batches = vec![empty_batch];
            let batches_iter = Box::new(RecordBatchIterator::new(
                batches.into_iter().map(Ok),
                schema.clone(),
            )) as Box<dyn RecordBatchReader + Send>;
            conn.create_table("behavioral_rules", batches_iter)
                .execute()
                .await?;
        }

        let table = conn.open_table("behavioral_rules").execute().await?;

        // 2. Generate Embedding for the constraint_text (or trigger_pattern + constraint_text)
        let text_to_embed = format!("{} {}", rule.trigger_pattern, rule.constraint_text);
        let mut embeddings = self
            .embedder
            .embed(vec![text_to_embed], None)
            .map_err(|e| anyhow::anyhow!("Fastembed error: {}", e))?;
        let vector = embeddings
            .pop()
            .unwrap_or_else(|| vec![0.0f32; self.dimensions]);

        // 3. Build Arrow Arrays
        let id_array = Arc::new(StringArray::from(vec![rule.id.as_str()]));
        let vector_list = Arc::new(FixedSizeListArray::try_new(
            Arc::new(Field::new("item", DataType::Float32, true)),
            self.dimensions as i32,
            Arc::new(Float32Array::from(vector)),
            None,
        )?);
        let version_array = Arc::new(UInt32Array::from(vec![rule.version]));
        let rule_class_array = Arc::new(StringArray::from(vec![rule.rule_class.as_str()]));
        let trigger_array = Arc::new(StringArray::from(vec![rule.trigger_pattern.as_str()]));
        let constraint_array = Arc::new(StringArray::from(vec![rule.constraint_text.as_str()]));
        let hit_count_array = Arc::new(UInt32Array::from(vec![rule.hit_count]));
        let status_array = Arc::new(StringArray::from(vec![rule.status.as_str()]));

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                id_array,
                vector_list,
                version_array,
                rule_class_array,
                trigger_array,
                constraint_array,
                hit_count_array,
                status_array,
            ],
        )?;

        let batches_iter = Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema.clone()))
            as Box<dyn RecordBatchReader + Send>;

        // Clean up old ID if it exists (upsert logic)
        table
            .delete(format!("id = '{}'", rule.id).as_str())
            .await
            .ok();
        table.add(batches_iter).execute().await?;

        Ok(())
    }

    pub async fn match_constraints(&self, payload: &str) -> anyhow::Result<Vec<BehavioralRule>> {
        let conn = lancedb::connect(&self.db_uri).execute().await?;

        let table = match conn.open_table("behavioral_rules").execute().await {
            Ok(t) => t,
            Err(e) => {
                info!(
                    "No behavioral rules table found. Returning empty constraints. Error: {}",
                    e
                );
                return Ok(vec![]);
            }
        };

        let mut embeddings = self
            .embedder
            .embed(vec![payload], None)
            .map_err(|e| anyhow::anyhow!("Fastembed error: {}", e))?;
        let embedding = embeddings
            .pop()
            .unwrap_or_else(|| vec![0.0f32; self.dimensions]);

        let mut stream = table
            .query()
            .nearest_to(embedding)?
            .limit(3)
            .execute()
            .await?;

        let mut rules = Vec::new();

        while let Some(batch) = stream.next().await {
            let batch: RecordBatch = batch?;
            if batch.num_rows() == 0 {
                continue;
            }

            let ids = batch
                .column_by_name("id")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let versions = batch
                .column_by_name("version")
                .unwrap()
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap();
            let classes = batch
                .column_by_name("rule_class")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let triggers = batch
                .column_by_name("trigger_pattern")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let constraints = batch
                .column_by_name("constraint_text")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let hit_counts = batch
                .column_by_name("hit_count")
                .unwrap()
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap();
            let statuses = batch
                .column_by_name("status")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();

            for i in 0..batch.num_rows() {
                rules.push(BehavioralRule {
                    id: ids.value(i).to_string(),
                    version: versions.value(i),
                    rule_class: classes.value(i).to_string(),
                    trigger_pattern: triggers.value(i).to_string(),
                    constraint_text: constraints.value(i).to_string(),
                    hit_count: hit_counts.value(i),
                    status: statuses.value(i).to_string(),
                });
            }
        }

        Ok(rules)
    }

    pub async fn evaluate_intent(
        &self,
        payload: &str,
        user_intent: &str,
    ) -> anyhow::Result<ValidateVerdict> {
        let res = evaluate_intent_embedded(payload, user_intent).await;

        match res {
            Ok(resp_text) => {
                // Try to parse the output as JSON
                if let Ok(parsed) = serde_json::from_str::<Value>(&resp_text) {
                    let approved = parsed["approved"].as_bool().unwrap_or(false);
                    let reason = parsed["reason"].as_str().unwrap_or("").to_string();

                    if approved {
                        return Ok(ValidateVerdict::SandboxPassHighFidelity {
                            notes: vec!["Semantic intent aligned".to_string()],
                        });
                    } else {
                        return Ok(ValidateVerdict::DeterministicReject {
                            reasons: vec![format!("Semantic mismatch: {}", reason)],
                            constraints: vec![],
                        });
                    }
                } else {
                    error!("Semantic validation API returned unparseable JSON: {}", resp_text);
                    return Ok(ValidateVerdict::ApprovedFailOpen {
                        warning: format!("Semantic validation API error: invalid format returned"),
                    });
                }
            }
            Err(e) => {
                error!("Semantic validation embedded model failed: {}", e);
                return Ok(ValidateVerdict::ApprovedFailOpen {
                    warning: "Semantic validation unavailable. Failing open.".to_string(),
                });
            }
        }
    }
}
