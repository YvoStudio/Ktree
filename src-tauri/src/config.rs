use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

/// 单个 VCS 绑定:把一个 git/svn 仓库的工作副本映射到 KB 的 src 子目录。
///
/// 凭证:`username` / `password` 留空时,完全依赖系统凭证管理
/// (git 走 ssh-agent / credential helper / GH CLI;svn 走 ~/.subversion/ 缓存)。
/// 填了用户名密码会通过 CLI 参数传给子进程,会出现在进程列表里,只在内网/可信环境用。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VcsBinding {
    /// "git" 或 "svn"
    pub vcs_type: String,
    /// 仓库 URL
    pub url: String,
    /// 相对 src 的子目录;空表示就用 src 本身
    #[serde(default)]
    pub sub_dir: String,
    /// 可选凭证:留空走系统凭证
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// 仅 git:checkout 的分支
    #[serde(default)]
    pub branch: String,
    /// 自动同步间隔(分钟);0 = 不自动
    #[serde(default)]
    pub sync_interval_minutes: u64,
}

/// 飞书同步凭证。每个知识库各自一份。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeishuConfig {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    /// 共享文件夹 token
    #[serde(default)]
    pub folder_token: String,
}

impl FeishuConfig {
    pub fn is_complete(&self) -> bool {
        !self.app_id.is_empty() && !self.app_secret.is_empty() && !self.folder_token.is_empty()
    }
}

/// 一个知识库。根目录下自动维护 src/ docs/ ref/ .ktree/ 四个子目录:
///   src/    用户源文件(用户可建文件夹、上传)
///   docs/   转换后的 Markdown(与 src 同相对路径,带 frontmatter)
///   ref/    转换产生的静态资源(图片等,与 src 同相对路径)
///   .ktree/ manifest.json / INDEX.md / KEYWORDS.md
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeBase {
    /// 标识 = 名称(URL 路径、SQLite / tantivy 关联都用它)。由后端从 name 填充。
    #[serde(default)]
    pub id: String,
    /// 知识库名称,唯一,既是显示名也是访问标识
    pub name: String,
    /// 知识库根目录
    pub root: PathBuf,
    #[serde(default)]
    pub feishu: FeishuConfig,
    #[serde(default)]
    pub vcs_bindings: Vec<VcsBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// HTTP 服务端口。0 = 自动(优先 80,失败回退 8080)
    #[serde(default)]
    pub http_port: u16,
    /// 飞书自动同步间隔(分钟)。0 = 不自动同步
    #[serde(default)]
    pub sync_interval_minutes: u64,
    /// 对外访问的自定义域名(如 `https://kb.example.com`),空表示不启用 ——
    /// 设置窗口的「web 界面」与 web UI 的「复制地址」会优先用它替代 `http://<ip>:<port>`。
    /// 不带 scheme 默认补 `http://`;末尾的 `/` 会被去掉。
    #[serde(default)]
    pub custom_domain: String,
    #[serde(default)]
    pub knowledge_bases: Vec<KnowledgeBase>,
}

impl AppConfig {
    /// 首次启动时给一个默认知识库,开箱即用。
    fn with_default_kb(data_dir: &std::path::Path) -> Self {
        Self {
            http_port: 0,
            sync_interval_minutes: 0,
            custom_domain: String::new(),
            knowledge_bases: vec![KnowledgeBase {
                id: String::new(),
                name: "默认知识库".to_string(),
                root: data_dir.join("kb"),
                feishu: FeishuConfig::default(),
                vcs_bindings: Vec::new(),
            }],
        }
    }
}

/// 知识库根目录下的固定子目录。
pub const KB_SUBDIRS: [&str; 4] = ["src", "docs", "ref", ".ktree"];

/// 与系统路由冲突的保留名,知识库不能用。
const RESERVED_NAMES: [&str; 5] = ["api", "lib", "mcp", "kb", "assets"];

/// 校验知识库名称:非空、不含路径分隔符、非保留字、不重复。供保存配置时使用。
fn validate_kbs(kbs: &[KnowledgeBase]) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for kb in kbs {
        let n = kb.name.trim();
        if n.is_empty() {
            anyhow::bail!("知识库名称不能为空");
        }
        if n.contains('/') || n.contains('\\') || n.contains("..") {
            anyhow::bail!("知识库名称「{n}」不能包含 / \\ 或 ..");
        }
        if RESERVED_NAMES.contains(&n.to_lowercase().as_str()) {
            anyhow::bail!("知识库名称「{n}」是系统保留字,请换一个");
        }
        if !seen.insert(n.to_string()) {
            anyhow::bail!("知识库名称「{n}」重复,名称必须唯一");
        }
    }
    Ok(())
}

/// 知识库 id 直接用 name(URL 路径、索引关联都用它)。
fn assign_ids(kbs: &mut [KnowledgeBase]) {
    for kb in kbs.iter_mut() {
        kb.name = kb.name.trim().to_string();
        kb.id = kb.name.clone();
    }
}

/// 为一个知识库创建 src/docs/ref/.ktree 目录结构。
fn ensure_kb_dirs(kb: &KnowledgeBase) -> std::io::Result<()> {
    for sub in KB_SUBDIRS {
        fs::create_dir_all(kb.root.join(sub))?;
    }
    Ok(())
}

pub struct ConfigStore {
    inner: Mutex<AppConfig>,
    file: PathBuf,
}

impl ConfigStore {
    pub fn load(app: &AppHandle) -> anyhow::Result<Self> {
        let cfg_dir = app
            .path()
            .app_config_dir()
            .map_err(|e| anyhow::anyhow!("app_config_dir: {e}"))?;
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| anyhow::anyhow!("app_data_dir: {e}"))?;
        fs::create_dir_all(&cfg_dir)?;
        fs::create_dir_all(&data_dir)?;

        let file = cfg_dir.join("config.json");
        let mut cfg: AppConfig = if file.exists() {
            let txt = fs::read_to_string(&file)?;
            serde_json::from_str(&txt).unwrap_or_else(|_| AppConfig::with_default_kb(&data_dir))
        } else {
            AppConfig::with_default_kb(&data_dir)
        };

        assign_ids(&mut cfg.knowledge_bases);
        for kb in &cfg.knowledge_bases {
            ensure_kb_dirs(kb)?;
        }
        fs::write(&file, serde_json::to_string_pretty(&cfg)?)?;

        Ok(Self {
            inner: Mutex::new(cfg),
            file,
        })
    }

    pub fn snapshot(&self) -> AppConfig {
        self.inner.lock().unwrap().clone()
    }

    /// 按 id 取知识库。
    pub fn get_kb(&self, id: &str) -> Option<KnowledgeBase> {
        self.inner
            .lock()
            .unwrap()
            .knowledge_bases
            .iter()
            .find(|k| k.id == id)
            .cloned()
    }

    pub fn replace(&self, mut new_cfg: AppConfig) -> anyhow::Result<()> {
        validate_kbs(&new_cfg.knowledge_bases)?;
        assign_ids(&mut new_cfg.knowledge_bases);
        for kb in &new_cfg.knowledge_bases {
            ensure_kb_dirs(kb)?;
        }
        fs::write(&self.file, serde_json::to_string_pretty(&new_cfg)?)?;
        *self.inner.lock().unwrap() = new_cfg;
        Ok(())
    }
}

#[tauri::command]
pub fn get_config(state: State<'_, crate::state::AppState>) -> AppConfig {
    state.config.snapshot()
}

#[tauri::command]
pub fn set_config(
    state: State<'_, crate::state::AppState>,
    config: AppConfig,
) -> Result<(), String> {
    state.config.replace(config).map_err(|e| e.to_string())
}
