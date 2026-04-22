use anyhow::{Context, Result};
use std::path::{PathBuf};
use std::fs;
use std::num::NonZero;
use tokio::io::AsyncWriteExt;
use serde::{Deserialize, Serialize};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::AddBos;

#[derive(Serialize, Deserialize, Clone)]
pub struct AcceptableModel {
    pub filename: String,
    pub download_url: String,
}

fn get_acceptable_models() -> Result<Vec<AcceptableModel>> {
    let config_dir = match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".config/NeuroCortex"),
        Err(_) => PathBuf::from(".neurocortex"),
    };

    if !config_dir.exists() {
        let _ = fs::create_dir_all(&config_dir);
    }

    let config_path = config_dir.join("models.json");

    if config_path.exists() {
        if let Ok(content) = fs::read_to_string(&config_path) {
            if let Ok(models) = serde_json::from_str::<Vec<AcceptableModel>>(&content) {
                if !models.is_empty() {
                    return Ok(models);
                }
            }
        }
    }

    // Default list if file doesn't exist or is empty
    let default_models = vec![
        AcceptableModel {
            filename: "qwen2.5-coder-7b-instruct-q4_k_m.gguf".to_string(),
            download_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/qwen2.5-coder-7b-instruct-q4_k_m.gguf".to_string(),
        }
    ];

    if let Ok(json_content) = serde_json::to_string_pretty(&default_models) {
        let _ = fs::write(&config_path, json_content);
    }

    Ok(default_models)
}

fn find_model_in_caches(filename: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let home_path = PathBuf::from(home);
    
    let cache_dirs = vec![
        home_path.join(".cache/huggingface/hub"),
        home_path.join(".ollama/models/blobs"),
        home_path.join(".cache/lm-studio/models"),
        home_path.join(".cache/neuro/models"),
    ];

    for dir in cache_dirs {
        if !dir.exists() { continue; }
        
        for entry in walkdir::WalkDir::new(&dir).follow_links(true).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    if name == filename {
                        return Some(entry.path().to_path_buf());
                    }
                }
            }
        }
    }
    None
}

async fn ensure_model_downloaded() -> Result<PathBuf> {
    let models_dir = match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".cache/neuro/models"),
        Err(_) => PathBuf::from(".neurocortex/models"),
    };

    if !models_dir.exists() {
        let _ = fs::create_dir_all(&models_dir);
    }

    let acceptable_models = get_acceptable_models()?;

    for model in &acceptable_models {
        if let Some(path) = find_model_in_caches(&model.filename) {
            return Ok(path);
        }
    }

    let default_model = &acceptable_models[0];
    let target_path = models_dir.join(&default_model.filename);

    let response = reqwest::get(&default_model.download_url).await.context("Failed to download model")?;
    if !response.status().is_success() {
        anyhow::bail!("Failed to download model: HTTP {}", response.status());
    }

    let mut file = tokio::fs::File::create(&target_path).await.context("Failed to create model file")?;
    let mut stream = response.bytes_stream();
    
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error reading streaming model download")?;
        file.write_all(&chunk).await.context("Failed to write to model file")?;
    }
    
    Ok(target_path)
}

pub async fn evaluate_intent_embedded(
    payload: &str,
    user_intent: &str,
) -> Result<String> {
    let actual_model_path = ensure_model_downloaded().await?;
    
    // We use a blocking task because llama.cpp initialization and decoding is synchronous CPU bound
    let payload = payload.to_string();
    let user_intent = user_intent.to_string();

    tokio::task::spawn_blocking(move || -> Result<String> {
        let backend = LlamaBackend::init().context("Failed to initialize llama backend")?;
        let model_params = LlamaModelParams::default();
        
        let model = LlamaModel::load_from_file(&backend, &actual_model_path, &model_params)
            .context("Failed to load model from file")?;

        let mut ctx_params = LlamaContextParams::default();
        ctx_params = ctx_params.with_n_ctx(Some(NonZero::new(2048).unwrap())); 
        
        let mut ctx = model.new_context(&backend, ctx_params)
            .context("Failed to create llama context")?;

        let system_prompt = "You are a strict guardrail for an AI agent. Does the proposed action align with the user intent? If no, explain why. Return ONLY a valid JSON object in the exact format: {\"approved\": true/false, \"reason\": \"your explanation\"}. Do not include markdown blocks like ```json.";
        let user_prompt = format!("User Intent: {}\n\nProposed Action: {}", user_intent, payload);
        
        let full_prompt = format!("<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n", system_prompt, user_prompt);
        
        let tokens = model.str_to_token(&full_prompt, AddBos::Always)
            .context("Failed to tokenize prompt")?;
            
        let mut batch = LlamaBatch::new(2048, 1);
        
        let last_index = tokens.len() - 1;
        for (i, &token) in tokens.iter().enumerate() {
            let is_last = i == last_index;
            batch.add(token, i as i32, &[0], is_last)?;
        }
        
        ctx.decode(&mut batch).context("llama.cpp decode failed")?;
        
        let mut generated_text = String::new();
        let mut n_cur = batch.n_tokens();
        let n_len = 512; 
        
        while n_cur < batch.n_tokens() + n_len {
            let candidates_iter = ctx.candidates();
            let mut best_id: Option<llama_cpp_2::token::LlamaToken> = None;
            let mut best_logit = f32::NEG_INFINITY;
            
            for candidate in candidates_iter {
                if candidate.logit() > best_logit {
                    best_logit = candidate.logit();
                    best_id = Some(candidate.id());
                }
            }
            
            let new_token_id = best_id.unwrap_or_else(|| model.token_eos());
            
            if new_token_id == model.token_eos() {
                break;
            }
            
            if let Ok(piece) = model.token_to_piece_bytes(new_token_id, 32, false, None) {
                let token_str = String::from_utf8_lossy(&piece).to_string();
                generated_text.push_str(&token_str);
            }
            
            batch.clear();
            batch.add(new_token_id, n_cur, &[0], true)?;
            ctx.decode(&mut batch)?;
            n_cur += 1;
        }
        
        Ok(generated_text.trim().to_string())
    }).await?
}
