//! 语义向量嵌入 —— 进程内 ONNX 推理(ort + tokenizers),加载 bge-small-zh-v1.5。
//!
//! 不再依赖 Node sidecar:模型(量化 ONNX + tokenizer.json)随发行版分发,
//! onnxruntime 由 `ort` crate 按目标平台处理。模型懒加载一次后复用。
//! 所有调用阻塞,放 `spawn_blocking` 里跑。

use std::path::PathBuf;
use std::sync::Mutex;

use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use tokenizers::Tokenizer;

/// bge 检索惯例:查询向量加这个指令前缀,文档向量不加。
pub const QUERY_INSTRUCTION: &str = "为这个句子生成表示以用于检索相关文章:";

/// bge-small 最大序列长度。
const MAX_LEN: usize = 512;

/// ort 的错误类型带非 Send 的上下文,无法直接 `?` 进 anyhow;统一转成字符串型错误。
fn oe<E: std::fmt::Display>(e: E) -> anyhow::Error {
    anyhow::anyhow!("onnx: {e}")
}

struct Model {
    tokenizer: Tokenizer,
    session: Session,
}

/// 进程内嵌入器。模型懒加载(首个请求触发)。
pub struct Embedder {
    model: Mutex<Option<Model>>,
}

impl Default for Embedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embedder {
    pub fn new() -> Self {
        Self {
            model: Mutex::new(None),
        }
    }

    /// 模型目录(内含 tokenizer.json 与 onnx/model_quantized.onnx)。
    /// 优先 KTREE_MODEL_DIR(打包态由 lib.rs 按 Tauri 资源目录设置);
    /// 否则开发期用源码树里的 src-tauri/resources/embed-model。
    fn model_dir() -> PathBuf {
        if let Ok(d) = std::env::var("KTREE_MODEL_DIR") {
            if !d.is_empty() {
                return PathBuf::from(d);
            }
        }
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/embed-model")
    }

    fn load() -> anyhow::Result<Model> {
        let dir = Self::model_dir();
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("加载 tokenizer 失败({}): {e}", dir.display()))?;
        let session = Session::builder()
            .map_err(oe)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(oe)?
            .commit_from_file(dir.join("onnx/model_quantized.onnx"))
            .map_err(oe)?;
        Ok(Model { tokenizer, session })
    }

    /// 把一批文本编码成单位长度句向量(CLS pooling + L2 归一化)。
    /// `instruction` 非空时前置到每条文本(查询用)。阻塞调用。
    pub fn embed(&self, texts: &[String], instruction: &str) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut guard = self.model.lock().unwrap();
        if guard.is_none() {
            *guard = Some(Self::load()?);
        }
        let model = guard.as_mut().unwrap();

        let inputs: Vec<String> = if instruction.is_empty() {
            texts.to_vec()
        } else {
            texts.iter().map(|t| format!("{instruction}{t}")).collect()
        };
        let encs = model
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(|e| anyhow::anyhow!("分词失败: {e}"))?;

        let batch = encs.len();
        let seq = encs
            .iter()
            .map(|e| e.get_ids().len().min(MAX_LEN))
            .max()
            .unwrap_or(1)
            .max(1);

        // 行优先填充到 [batch, seq],右侧 pad 0,attention_mask 标真实 token。
        let mut ids = vec![0i64; batch * seq];
        let mut mask = vec![0i64; batch * seq];
        let types = vec![0i64; batch * seq]; // 单句:token_type 全 0
        for (b, e) in encs.iter().enumerate() {
            let eids = e.get_ids();
            let emask = e.get_attention_mask();
            let n = eids.len().min(seq);
            for j in 0..n {
                ids[b * seq + j] = eids[j] as i64;
                mask[b * seq + j] = emask[j] as i64;
            }
        }

        let shape = [batch, seq];
        let t_ids = Tensor::from_array((shape, ids)).map_err(oe)?;
        let t_mask = Tensor::from_array((shape, mask)).map_err(oe)?;
        let t_types = Tensor::from_array((shape, types)).map_err(oe)?;
        let outputs = model
            .session
            .run(ort::inputs![
                "input_ids" => t_ids,
                "attention_mask" => t_mask,
                "token_type_ids" => t_types,
            ])
            .map_err(oe)?;

        let (oshape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(oe)?;
        // last_hidden_state: [batch, seq, hidden]
        let hidden = *oshape.last().unwrap() as usize;

        let mut out = Vec::with_capacity(batch);
        for b in 0..batch {
            // CLS pooling:取第 0 个 token 的隐藏向量
            let base = b * seq * hidden;
            let mut v: Vec<f32> = data[base..base + hidden].to_vec();
            // L2 归一化 → 单位向量,cosine 相似度即点积
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut v {
                    *x /= norm;
                }
            }
            out.push(v);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 嵌入维度与归一化正确,且与 transformers.js 参考向量一致(同模型应几乎相同)。
    #[test]
    fn embed_matches_reference() {
        let e = Embedder::new();
        let v = e.embed(&["配置表规范".to_string()], "").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].len(), 512, "bge-small 应为 512 维");
        // 单位向量
        let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "应为单位向量,实际 {norm}");
        // 与 transformers.js 对同一文本的输出前几维对比(CLS pooling + normalize)
        let reference = [
            -0.0210349652916193f32,
            0.018730543553829193,
            0.0018874045927077532,
            0.024844670668244362,
            -0.00011936538066947833,
        ];
        for (i, r) in reference.iter().enumerate() {
            assert!(
                (v[0][i] - r).abs() < 0.02,
                "第 {i} 维偏差过大: rust={} ref={}",
                v[0][i],
                r
            );
        }
    }
}
