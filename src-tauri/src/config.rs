use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

/// 单个 VCS 绑定:把一个 git/svn 仓库镜像到 KB 的 src/vcs/<name>/ 目录。
/// 该目录只读 + 严格镜像:仓库里没有的文件同步时会被清掉。
///
/// 凭证:`username` / `password` 留空时,完全依赖系统凭证管理
/// (git 走 ssh-agent / credential helper / GH CLI;svn 走 ~/.subversion/ 缓存)。
/// 填了用户名密码会通过 CLI 参数传给子进程,会出现在进程列表里,只在内网/可信环境用。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VcsBinding {
    /// 绑定名 = src/vcs/ 下的目录名。必填,库内唯一,创建后不可改。
    #[serde(default)]
    pub name: String,
    /// "git" 或 "svn"
    pub vcs_type: String,
    /// 仓库 URL
    pub url: String,
    /// 可选凭证:留空走系统凭证
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// 仅 git:checkout 的分支
    #[serde(default)]
    pub branch: String,
    /// 仅 git:只同步仓库内的这个子目录(空 = 整个仓库)。
    /// 用稀疏检出实现,内容扁平映射到 src/vcs/<name>/,不带这一层路径。
    /// (svn 不用此字段,直接把 url 指到子目录即可)
    #[serde(default)]
    pub repo_sub_path: String,
    /// 自动同步间隔(分钟);0 = 不自动
    #[serde(default)]
    pub sync_interval_minutes: u64,
}

/// 单个云文档绑定:把一个云端文档源(飞书文件夹 / 单篇文档)镜像到
/// KB 的 src/cloud/<provider>/<name>/ 目录。该目录只读 + 严格镜像。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudBinding {
    /// 绑定名 = src/cloud/<provider>/ 下的目录名。必填,库内唯一,创建后不可改。
    #[serde(default)]
    pub name: String,
    /// 提供方,目前只支持 "feishu"
    #[serde(default)]
    pub provider: String,
    /// 飞书应用凭证
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    /// 目标类型:"folder"(共享文件夹,递归)或 "doc"(单篇文档)
    #[serde(default)]
    pub target_type: String,
    /// folder_token 或 doc_token
    #[serde(default)]
    pub target_token: String,
    /// 自动同步间隔(分钟);0 = 不自动
    #[serde(default)]
    pub sync_interval_minutes: u64,
}

impl CloudBinding {
    pub fn is_complete(&self) -> bool {
        !self.app_id.is_empty() && !self.app_secret.is_empty() && !self.target_token.is_empty()
    }
}

/// 一个知识库。根目录下自动维护三样东西:
///   src/    源文件,分三区:
///           upload/                用户上传区(可建文件夹、上传、删除)
///           vcs/<绑定名>/           仓库镜像区(只读,严格镜像)
///           cloud/<提供方>/<绑定名>/ 云文档镜像区(只读,严格镜像)
///   docs/   阅读视图:转换后的 Markdown(与 src 同相对路径),
///           图片附件放 md 旁边的同名 .assets/ 伴生目录
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
    pub vcs_bindings: Vec<VcsBinding>,
    #[serde(default)]
    pub cloud_bindings: Vec<CloudBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// HTTP 服务端口。0 = 自动(优先 80,失败回退 8080)
    #[serde(default)]
    pub http_port: u16,
    /// 对外访问的自定义域名(如 `https://kb.example.com`),空表示不启用 ——
    /// 设置窗口的「web 界面」与 web UI 的「复制地址」会优先用它替代 `http://<ip>:<port>`。
    /// 不带 scheme 默认补 `http://`;末尾的 `/` 会被去掉。
    #[serde(default)]
    pub custom_domain: String,
    /// web UI 左上角与浏览器标题显示的站点名,空 = 回退到 "Ktree"。便于统一源对外分享时打自己的名。
    #[serde(default)]
    pub site_name: String,
    #[serde(default)]
    pub knowledge_bases: Vec<KnowledgeBase>,
}

impl AppConfig {
    /// 首次启动时给一个默认知识库,开箱即用。
    fn with_default_kb(data_dir: &std::path::Path) -> Self {
        Self {
            http_port: 0,
            custom_domain: String::new(),
            site_name: String::new(),
            knowledge_bases: vec![KnowledgeBase {
                id: String::new(),
                name: "默认知识库".to_string(),
                root: data_dir.join("kb"),
                vcs_bindings: Vec::new(),
                cloud_bindings: Vec::new(),
            }],
        }
    }
}

/// 知识库根目录下的固定子目录。
/// 注意:src/vcs 与 src/cloud 不在这里 —— 它们在添加绑定后由首次同步按需创建,
/// 没有绑定的库不该出现空的镜像区目录。
pub const KB_SUBDIRS: [&str; 4] = ["src", "src/upload", "docs", ".ktree"];

/// src 下的三个内容区。
pub const AREA_UPLOAD: &str = "upload";
pub const AREA_VCS: &str = "vcs";
pub const AREA_CLOUD: &str = "cloud";

/// 与系统路由冲突的保留名,知识库不能用。
const RESERVED_NAMES: [&str; 5] = ["api", "lib", "mcp", "kb", "assets"];

/// 校验一个绑定名:非空、不含路径分隔符 / ..、不以 .assets 结尾(资源伴生目录后缀)。
fn validate_binding_name(name: &str, kind: &str) -> anyhow::Result<()> {
    let n = name.trim();
    if n.is_empty() {
        anyhow::bail!("{kind}绑定必须有名字(将作为目录名)");
    }
    if n.contains('/') || n.contains('\\') || n.contains("..") || n.starts_with('.') {
        anyhow::bail!("{kind}绑定名「{n}」不能包含 / \\ .. 或以 . 开头");
    }
    if n.ends_with(".assets") {
        anyhow::bail!("{kind}绑定名「{n}」不能以 .assets 结尾(系统保留后缀)");
    }
    Ok(())
}

/// 校验知识库名称与绑定:名称非空、不含路径分隔符、非保留字、不重复;
/// 各绑定名合法且在库内唯一。供保存配置时使用。
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

        // VCS 绑定名唯一性
        let mut vcs_names = HashSet::new();
        for b in &kb.vcs_bindings {
            validate_binding_name(&b.name, "VCS ")?;
            if !vcs_names.insert(b.name.trim().to_string()) {
                anyhow::bail!("知识库「{n}」的 VCS 绑定名「{}」重复", b.name);
            }
        }
        // 云文档绑定名唯一性(同 provider 下唯一)
        let mut cloud_names = HashSet::new();
        for b in &kb.cloud_bindings {
            validate_binding_name(&b.name, "云文档")?;
            if !cloud_names.insert(format!("{}/{}", b.provider, b.name.trim())) {
                anyhow::bail!("知识库「{n}」的云文档绑定名「{}」重复", b.name);
            }
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

/// 为一个知识库创建 src(含三区)/docs/.ktree 目录结构。
fn ensure_kb_dirs(kb: &KnowledgeBase) -> std::io::Result<()> {
    for sub in KB_SUBDIRS {
        fs::create_dir_all(kb.root.join(sub))?;
    }
    Ok(())
}

pub struct ConfigStore {
    inner: Mutex<AppConfig>,
    file: PathBuf,
    /// 应用数据目录 —— 新增知识库未指定 root 时落在这里。
    data_dir: PathBuf,
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

        // 旧版配置清洗:三区重构前的 VCS 绑定没有 name 字段(只有 sub_dir),
        // 云文档绑定不存在。带不合法名字的绑定直接丢弃,让用户在新结构下重建。
        for kb in &mut cfg.knowledge_bases {
            let before = kb.vcs_bindings.len();
            kb.vcs_bindings
                .retain(|b| validate_binding_name(&b.name, "VCS ").is_ok());
            kb.cloud_bindings
                .retain(|b| validate_binding_name(&b.name, "云文档").is_ok());
            if kb.vcs_bindings.len() < before {
                eprintln!(
                    "[ktree] 知识库「{}」存在旧版(无名字)VCS 绑定,已丢弃,请重新添加",
                    kb.name
                );
            }
        }

        assign_ids(&mut cfg.knowledge_bases);
        for kb in &cfg.knowledge_bases {
            ensure_kb_dirs(kb)?;
        }
        fs::write(&file, serde_json::to_string_pretty(&cfg)?)?;

        Ok(Self {
            inner: Mutex::new(cfg),
            file,
            data_dir,
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

    // ----- 受限的细粒度改配置接口(供 REST / MCP 给 AI 用)-----
    // 只允许:新增知识库、管理 VCS / 云文档绑定;
    // 不允许:删除知识库、改 HTTP 端口、改自定义域名 —— 那些仍只走桌面设置窗。

    /// VcsBinding 最小校验:名字合法,类型必须 git/svn,url 非空。
    fn validate_vcs(b: &VcsBinding) -> anyhow::Result<()> {
        validate_binding_name(&b.name, "VCS ")?;
        if b.vcs_type != "git" && b.vcs_type != "svn" {
            anyhow::bail!("VCS 类型必须是 git 或 svn,收到「{}」", b.vcs_type);
        }
        if b.url.trim().is_empty() {
            anyhow::bail!("VCS 仓库 URL 不能为空");
        }
        Ok(())
    }

    /// CloudBinding 最小校验:名字合法,provider 支持,凭证 / token 非空。
    fn validate_cloud(b: &CloudBinding) -> anyhow::Result<()> {
        validate_binding_name(&b.name, "云文档")?;
        if b.provider != "feishu" {
            anyhow::bail!("云文档提供方目前只支持 feishu,收到「{}」", b.provider);
        }
        if b.target_type != "folder" && b.target_type != "doc" {
            anyhow::bail!("云文档目标类型必须是 folder 或 doc,收到「{}」", b.target_type);
        }
        if !b.is_complete() {
            anyhow::bail!("云文档绑定的 app_id / app_secret / target_token 都不能为空");
        }
        Ok(())
    }

    /// 新增一个知识库。`root` 为 None 时落在应用数据目录下的 `kb-<name>`。
    /// 仅新增,不提供删除。
    pub fn add_kb(&self, name: &str, root: Option<PathBuf>) -> anyhow::Result<KnowledgeBase> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("知识库名称不能为空");
        }
        let root = root.unwrap_or_else(|| self.data_dir.join(format!("kb-{name}")));
        let mut cfg = self.snapshot();
        cfg.knowledge_bases.push(KnowledgeBase {
            id: String::new(),
            name: name.clone(),
            root,
            vcs_bindings: Vec::new(),
            cloud_bindings: Vec::new(),
        });
        self.replace(cfg)?; // 内含 validate_kbs(查重 / 保留字)+ 建目录 + 落盘
        self.get_kb(&name)
            .ok_or_else(|| anyhow::anyhow!("新增知识库后无法读回"))
    }

    fn with_kb_mut<R>(
        &self,
        kb_id: &str,
        f: impl FnOnce(&mut KnowledgeBase) -> anyhow::Result<R>,
    ) -> anyhow::Result<R> {
        let mut cfg = self.snapshot();
        let kb = cfg
            .knowledge_bases
            .iter_mut()
            .find(|k| k.id == kb_id)
            .ok_or_else(|| anyhow::anyhow!("知识库「{kb_id}」不存在"))?;
        let r = f(kb)?;
        self.replace(cfg)?;
        Ok(r)
    }

    // ----- VCS 绑定 -----

    /// 给某知识库追加一条 VCS 绑定,返回新绑定的下标。
    pub fn add_vcs_binding(&self, kb_id: &str, b: VcsBinding) -> anyhow::Result<usize> {
        Self::validate_vcs(&b)?;
        self.with_kb_mut(kb_id, |kb| {
            kb.vcs_bindings.push(b);
            Ok(kb.vcs_bindings.len() - 1)
        })
    }

    /// 覆盖某知识库第 `idx` 条 VCS 绑定。绑定名(=目录名)不允许改。
    pub fn update_vcs_binding(
        &self,
        kb_id: &str,
        idx: usize,
        b: VcsBinding,
    ) -> anyhow::Result<()> {
        Self::validate_vcs(&b)?;
        self.with_kb_mut(kb_id, |kb| {
            let old = kb
                .vcs_bindings
                .get(idx)
                .ok_or_else(|| {
                    anyhow::anyhow!("VCS 绑定下标 {idx} 越界(共 {} 条)", kb.vcs_bindings.len())
                })?;
            if old.name.trim() != b.name.trim() {
                anyhow::bail!("VCS 绑定名(目录名)不可修改,只能删除后重建");
            }
            kb.vcs_bindings[idx] = b;
            Ok(())
        })
    }

    /// 删除某知识库第 `idx` 条 VCS 绑定(仅删配置;目录与文档清理由调用方负责)。
    pub fn remove_vcs_binding(&self, kb_id: &str, idx: usize) -> anyhow::Result<VcsBinding> {
        self.with_kb_mut(kb_id, |kb| {
            if idx >= kb.vcs_bindings.len() {
                anyhow::bail!("VCS 绑定下标 {idx} 越界(共 {} 条)", kb.vcs_bindings.len());
            }
            Ok(kb.vcs_bindings.remove(idx))
        })
    }

    // ----- 云文档绑定 -----

    /// 给某知识库追加一条云文档绑定,返回新绑定的下标。
    pub fn add_cloud_binding(&self, kb_id: &str, b: CloudBinding) -> anyhow::Result<usize> {
        Self::validate_cloud(&b)?;
        self.with_kb_mut(kb_id, |kb| {
            kb.cloud_bindings.push(b);
            Ok(kb.cloud_bindings.len() - 1)
        })
    }

    /// 覆盖某知识库第 `idx` 条云文档绑定。绑定名(=目录名)不允许改。
    pub fn update_cloud_binding(
        &self,
        kb_id: &str,
        idx: usize,
        b: CloudBinding,
    ) -> anyhow::Result<()> {
        Self::validate_cloud(&b)?;
        self.with_kb_mut(kb_id, |kb| {
            let old = kb
                .cloud_bindings
                .get(idx)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "云文档绑定下标 {idx} 越界(共 {} 条)",
                        kb.cloud_bindings.len()
                    )
                })?;
            if old.name.trim() != b.name.trim() {
                anyhow::bail!("云文档绑定名(目录名)不可修改,只能删除后重建");
            }
            kb.cloud_bindings[idx] = b;
            Ok(())
        })
    }

    /// 删除某知识库第 `idx` 条云文档绑定(仅删配置;目录与文档清理由调用方负责)。
    pub fn remove_cloud_binding(&self, kb_id: &str, idx: usize) -> anyhow::Result<CloudBinding> {
        self.with_kb_mut(kb_id, |kb| {
            if idx >= kb.cloud_bindings.len() {
                anyhow::bail!(
                    "云文档绑定下标 {idx} 越界(共 {} 条)",
                    kb.cloud_bindings.len()
                );
            }
            Ok(kb.cloud_bindings.remove(idx))
        })
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
