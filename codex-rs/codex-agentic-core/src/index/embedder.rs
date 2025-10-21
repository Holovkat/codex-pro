use anyhow::Result;
use anyhow::anyhow;
use fastembed::EmbeddingModel;
use fastembed::InitOptions;
use fastembed::TextEmbedding;

pub struct EmbeddingHandle {
    pub model: TextEmbedding,
    model_name: String,
}

impl EmbeddingHandle {
    pub fn new(selected: Option<EmbeddingModel>) -> Result<Self> {
        let model_choice = selected.unwrap_or_default();
        let model = TextEmbedding::try_new(InitOptions::new(model_choice.clone()))
            .or_else(|_| TextEmbedding::try_new(Default::default()))?;
        let model_name = format!("{model_choice:?}");
        Ok(Self { model, model_name })
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.model
            .embed(texts, None)
            .map_err(|err| anyhow!("failed to embed batch: {err}"))
    }
}

pub fn parse_model(name: &str) -> Option<EmbeddingModel> {
    name.parse::<EmbeddingModel>().ok()
}
