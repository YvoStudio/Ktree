// Ktree — 跨平台知识库服务
//
// 知识库目录结构:src/(upload 上传区 + vcs 仓库镜像区 + cloud 云文档镜像区)
//              + docs/(Markdown 阅读视图 + .assets 伴生资源)+ .ktree/(元数据)
//
// 模块规划:
//   config     配置:知识库根目录、HTTP 端口、VCS / 云文档绑定
//   store      SQLite 元数据:documents / notes / 向量
//   convert    调 Node sidecar 把文档转 Markdown
//   index      tantivy 全文索引:建索引 / BM25 搜索
//   http       axum REST API,绑 0.0.0.0
//   mcp        MCP server:HTTP transport
//   vcs        git / svn 仓库严格镜像同步(src/vcs/<绑定名>/)
//   feishu     飞书云文档严格镜像同步(src/cloud/feishu/<绑定名>/)
//   scheduler  定时同步循环(VCS + 云文档绑定)
//   commands   Tauri invoke 命令(GUI 用)

mod commands;
mod config;
mod convert;
mod embed;
mod feishu;
mod http;
mod index;
mod ingest;
mod kbmeta;
mod mcp;
mod scheduler;
mod search;
mod textproc;
mod vcs;
mod state;
mod store;

use std::sync::{Arc, Mutex};

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

/// 显示并聚焦设置窗口。
fn show_settings_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // macOS:不显示 Dock 图标,纯菜单栏托盘应用
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // 告知嵌入器随包模型目录(Tauri 资源:resources/embed-model)。
            // 开发期无需设置,embed.rs 回退到源码树 resources/embed-model。
            if let Ok(res) = app.path().resource_dir() {
                let md = res.join("resources").join("embed-model");
                if md.join("tokenizer.json").exists() {
                    std::env::set_var("KTREE_MODEL_DIR", md);
                }
            }

            // 配置与存储层
            let data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| anyhow::anyhow!("app_data_dir: {e}"))?;
            let cfg_store = Arc::new(config::ConfigStore::load(app.handle())?);
            let store = Arc::new(store::Store::open(&data_dir.join("ktree.db"))?);
            let search_index =
                Arc::new(index::SearchIndex::open(&data_dir.join("index"))?);
            let app_state = state::AppState {
                config: cfg_store,
                store,
                index: search_index,
                embedder: Arc::new(embed::Embedder::new()),
                http_port: Arc::new(Mutex::new(None)),
                last_vcs_sync: Arc::new(Mutex::new(std::collections::HashMap::new())),
                last_cloud_sync: Arc::new(Mutex::new(std::collections::HashMap::new())),
                syncing: Arc::new(Mutex::new(std::collections::HashSet::new())),
            };
            app.manage(app_state.clone());

            // 恢复历次同步状态(SQLite → 内存 map),webui 重启后仍能看到最近同步时间
            app_state.restore_sync_states();

            // 启动时:清理孤儿 → 按 manifest 重建缓存 → 给存量文档补算语义向量
            let rebuild_state = app_state.clone();
            tauri::async_runtime::spawn(async move {
                // 清理改名 / 删库遗留的孤儿文档记录(SQLite + tantivy)
                let valid: Vec<String> = rebuild_state
                    .config
                    .snapshot()
                    .knowledge_bases
                    .iter()
                    .map(|k| k.id.clone())
                    .collect();
                if let Ok(ids) = rebuild_state.store.orphan_doc_ids(&valid) {
                    if !ids.is_empty() {
                        for id in &ids {
                            let _ = rebuild_state.store.delete_document(*id);
                            let _ = rebuild_state.index.delete(*id);
                        }
                        let _ = rebuild_state.index.commit();
                        println!("[ktree] 清理孤儿文档记录 {} 条", ids.len());
                    }
                }

                for kb in rebuild_state.config.snapshot().knowledge_bases {
                    let st = rebuild_state.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        match kbmeta::rebuild_cache_if_needed(&st, &kb) {
                            Ok(n) if n > 0 => println!(
                                "[ktree] 知识库「{}」从 manifest 重建缓存 {} 篇",
                                kb.name, n
                            ),
                            Ok(_) => {}
                            Err(e) => eprintln!(
                                "[ktree] 知识库「{}」缓存重建失败: {e}",
                                kb.name
                            ),
                        }
                    })
                    .await;
                }

                // 给还没有语义向量的存量文档后台补算
                let st = rebuild_state.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let (done, failed) = ingest::backfill_vectors(&st);
                    if done > 0 || failed > 0 {
                        println!("[ktree] 语义向量补算完成:成功 {done} 失败 {failed}");
                    }
                })
                .await;

                // 给摘要为空的存量文档补算摘要 / 关键词,并用纯文本重建其索引
                let st = rebuild_state.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let n = ingest::backfill_meta(&st);
                    if n > 0 {
                        println!("[ktree] 摘要 / 关键词补算完成:{n} 篇");
                    }
                })
                .await;
            });

            // 启动 HTTP API 服务(局域网可访问)
            let http_state = app_state.clone();
            tauri::async_runtime::spawn(async move {
                http::serve(http_state).await;
            });

            // 启动飞书定时同步循环
            let sched_state = app_state.clone();
            tauri::async_runtime::spawn(async move {
                scheduler::run(sched_state).await;
            });

            // 系统托盘:只保留「设置」「退出」
            let settings_item =
                MenuItem::with_id(app, "tray_settings", "设置", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "tray_quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&settings_item, &quit_item])?;

            let tray_png =
                image::load_from_memory(include_bytes!("../icons/tray.png"))?.to_rgba8();
            let (tw, th) = (tray_png.width(), tray_png.height());
            let tray_icon = tauri::image::Image::new_owned(tray_png.into_raw(), tw, th);

            TrayIconBuilder::with_id("main-tray")
                .icon(tray_icon)
                .icon_as_template(true)
                .tooltip("Ktree 知识库服务")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    // 左键单击托盘图标 → 打开设置窗口
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_settings_window(tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "tray_settings" => show_settings_window(app),
                    "tray_quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // 关闭按钮 → 隐藏到托盘,服务驻留后台;真正退出走托盘菜单
            if let Some(window) = app.get_webview_window("main") {
                let w = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::get_config,
            config::set_config,
            commands::get_service_info,
            commands::delete_binding,
            commands::check_vcs_all,
            commands::get_local_ip,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
