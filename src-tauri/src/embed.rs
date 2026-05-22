//! 语义向量嵌入客户端 —— 驱动常驻的 Node `embed.js` sidecar。
//!
//! sidecar 启动一次、模型加载一次,之后请求-应答复用。本客户端懒启动子进程,
//! 进程崩了下次调用自动重启。所有调用阻塞,放 `spawn_blocking` 里跑。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use serde::Deserialize;

/// bge 检索惯例:查询向量加这个指令前缀,文档向量不加。
pub const QUERY_INSTRUCTION: &str = "为这个句子生成表示以用于检索相关文章:";

#[derive(Deserialize)]
struct EmbedResponse {
    ok: bool,
    #[serde(default)]
    vectors: Vec<Vec<f32>>,
    #[serde(default)]
    error: String,
}

/// 持有常驻 sidecar 子进程及其 stdin/stdout 管道。
struct EmbedProc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for EmbedProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 常驻 embed sidecar 的客户端。
pub struct Embedder {
    proc: Mutex<Option<EmbedProc>>,
}

impl Default for Embedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embedder {
    pub fn new() -> Self {
        Self {
            proc: Mutex::new(None),
        }
    }

    /// 开发期:`node <project>/sidecar/embed.js`;打包后:同目录的 `embed` 二进制。
    fn sidecar_command() -> (String, Vec<String>) {
        #[cfg(debug_assertions)]
        {
            let script: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .map(|p| p.join("sidecar").join("embed.js"))
                .unwrap_or_else(|| PathBuf::from("sidecar/embed.js"));
            (
                "node".to_string(),
                vec![script.to_string_lossy().into_owned()],
            )
        }
        #[cfg(not(debug_assertions))]
        {
            let bin = crate::convert::sidecar_binary_path("embed");
            (bin.to_string_lossy().into_owned(), Vec::new())
        }
    }

    fn spawn() -> anyhow::Result<EmbedProc> {
        let (program, args) = Self::sidecar_command();
        let mut child = Command::new(&program)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // 模型加载 / 下载日志不要污染
            .spawn()
            .map_err(|e| anyhow::anyhow!("启动 embed sidecar 失败({program}): {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 embed stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 embed stdout"))?;
        Ok(EmbedProc {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// 把一批文本编码成向量。`instruction` 非空时前置到每条文本(查询用)。
    /// 阻塞调用 —— 放 `spawn_blocking` 里跑。
    pub fn embed(&self, texts: &[String], instruction: &str) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let req = serde_json::to_string(&serde_json::json!({
            "texts": texts,
            "instruction": instruction,
        }))?;

        let mut guard = self.proc.lock().unwrap();
        let mut last_err = None;
        // 试一次;管道坏了就重启再试一次。
        for _ in 0..2 {
            if guard.is_none() {
                match Self::spawn() {
                    Ok(p) => *guard = Some(p),
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                }
            }
            match Self::round_trip(guard.as_mut().unwrap(), &req) {
                Ok(resp) => {
                    if !resp.ok {
                        anyhow::bail!("embed sidecar 报错: {}", resp.error);
                    }
                    if resp.vectors.len() != texts.len() {
                        anyhow::bail!(
                            "embed 返回向量数 {} 与文本数 {} 不符",
                            resp.vectors.len(),
                            texts.len()
                        );
                    }
                    return Ok(resp.vectors);
                }
                Err(e) => {
                    *guard = None; // 丢弃坏进程,下轮重启
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("embed 调用失败")))
    }

    /// 一次请求-应答:发一行 JSON,收一行 JSON。
    fn round_trip(proc: &mut EmbedProc, req: &str) -> anyhow::Result<EmbedResponse> {
        proc.stdin.write_all(req.as_bytes())?;
        proc.stdin.write_all(b"\n")?;
        proc.stdin.flush()?;
        let mut line = String::new();
        let n = proc.stdout.read_line(&mut line)?;
        if n == 0 {
            anyhow::bail!("embed sidecar 关闭了输出(可能启动失败)");
        }
        Ok(serde_json::from_str(line.trim())?)
    }
}
