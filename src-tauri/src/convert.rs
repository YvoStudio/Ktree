use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct ConvertRequest<'a> {
    input: &'a str,
    ext: &'a str,
    /// 转换产生的图片等静态资源的输出目录(绝对路径)
    ref_dir: &'a str,
    /// Markdown 中引用上述资源时用的相对路径前缀
    ref_prefix: &'a str,
}

/// Node sidecar 的转换结果。ok=false 时看 error。
#[derive(Debug, Clone, Deserialize)]
pub struct ConvertResult {
    pub ok: bool,
    #[serde(default)]
    pub markdown: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub error: String,
}

/// Tauri externalBin 把打包好的 sidecar 二进制放在主程序同目录,文件名去掉了 triple 后缀。
#[cfg(not(debug_assertions))]
pub(crate) fn sidecar_binary_path(name: &str) -> PathBuf {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(format!("{name}{ext}"))))
        .unwrap_or_else(|| PathBuf::from(format!("{name}{ext}")))
}

/// 解析 sidecar 的调用方式。
/// 开发期(debug):`node <project>/sidecar/convert.js`
/// 打包后(release):主程序同目录的自包含二进制 `convert`。
fn sidecar_command() -> (String, Vec<String>) {
    #[cfg(debug_assertions)]
    {
        let script: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.join("sidecar").join("convert.js"))
            .unwrap_or_else(|| PathBuf::from("sidecar/convert.js"));
        ("node".to_string(), vec![script.to_string_lossy().into_owned()])
    }
    #[cfg(not(debug_assertions))]
    {
        let bin = sidecar_binary_path("convert");
        (bin.to_string_lossy().into_owned(), Vec::new())
    }
}

/// 把任意支持的文档转成 Markdown。失败(spawn 失败、非零退出)返回 Err;
/// 文档本身无法转换(格式不支持等)返回 Ok(ConvertResult{ ok:false }).
/// `ref_dir` 是图片等资源的输出目录,`ref_prefix` 是 md 中引用资源的相对路径前缀。
pub fn convert_file(
    input: &Path,
    ext: &str,
    ref_dir: &Path,
    ref_prefix: &str,
) -> anyhow::Result<ConvertResult> {
    let input_str = input
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("文件路径含非法 UTF-8: {input:?}"))?;
    let ref_dir_str = ref_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("ref 目录路径含非法 UTF-8: {ref_dir:?}"))?;
    let req = serde_json::to_string(&ConvertRequest {
        input: input_str,
        ext,
        ref_dir: ref_dir_str,
        ref_prefix,
    })?;

    let (program, args) = sidecar_command();
    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("启动转换 sidecar 失败({program}): {e}"))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 sidecar stdin"))?;
        stdin.write_all(req.as_bytes())?;
    } // drop stdin → 子进程收到 EOF

    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "转换 sidecar 异常退出: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let result: ConvertResult = serde_json::from_slice(&output.stdout).map_err(|e| {
        anyhow::anyhow!(
            "解析 sidecar 输出失败: {e}; 原始输出: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;
    Ok(result)
}
