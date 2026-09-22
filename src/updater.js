// Ktree auto-updater
// 直接调用 Tauri plugin-updater + plugin-process 的 invoke 通道,不依赖打包。
(function () {
  const DISMISS_KEY = 'ktree-updater:dismissed-version';

  /// 初始化。window.__TAURI__ 由 withGlobalTauri 注入,正常在脚本执行前就绪;
  /// 万一没赶上(head 里同步加载的时序),退回 DOMContentLoaded 再试一次。
  function init() {
    const TAURI = window.__TAURI__;
    if (!TAURI || !TAURI.core) return false;
    const { invoke, Channel } = TAURI.core;

    // 公开 API:
    //   silent   默认 true(启动时静默检查,不打扰)。手动检查传 false。
    //   onResult 可选回调,拿到结论 {ok, available, version?, currentVersion?, error?},
    //            由调用方决定怎么展示(设置窗口用弹窗/toast,不再走 alert)。
    window.KtreeUpdater = {
      check: (opts = {}) => checkForUpdates({
        silent: opts.silent !== false,
        onResult: typeof opts.onResult === 'function' ? opts.onResult : null,
      }),
    };

    async function checkForUpdates({ silent, onResult }) {
      const emit = (res) => {
        if (!onResult) return;
        try { onResult(res); } catch (e) { console.warn('[ktree] updater onResult 抛错', e); }
      };

      let result;
      try {
        result = await invoke('plugin:updater|check');
      } catch (err) {
        const msg = friendlyUpdateError(err);
        emit({ ok: false, error: msg });
        // 没给回调才自己提示(避免和宿主提示重复)
        if (!onResult && !silent) alert('检查更新失败: ' + msg);
        return;
      }
      if (!result || result.available === false) {
        emit({ ok: true, available: false, currentVersion: result && result.currentVersion });
        if (!onResult && !silent) alert('当前已是最新版本');
        return;
      }
      emit({ ok: true, available: true, version: result.version, currentVersion: result.currentVersion });
      if (silent && localStorage.getItem(DISMISS_KEY) === result.version) return;
      showDialog(result);
    }

    /// 更新检查失败十有八九是网络(内网机器访问不到 GitHub)。
    /// 原始报错留全,再补一句人话 —— 否则只有 "error sending request for url ..."。
    function friendlyUpdateError(err) {
      const raw = (err && err.message) ? err.message : String(err);
      if (/dns|resolve|connect|network|timed?\s?out|sending request|tls|handshake|443/i.test(raw)) {
        return raw + '\n\n像是网络原因:本机可能访问不到 GitHub。内网机器可以改用内网更新源。';
      }
      return raw;
    }

    function showDialog(info) {
      const css = `
      .hsu-overlay { position: fixed; inset: 0; z-index: 9999; background: rgba(0,0,0,0.5); display: flex; align-items: center; justify-content: center; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; }
      .hsu-dialog { width: 480px; max-width: 90vw; background: #fff; color: #1a1a1a; border-radius: 10px; box-shadow: 0 20px 60px rgba(0,0,0,0.4); padding: 20px 22px; font-size: 13px; }
      @media (prefers-color-scheme: dark) { .hsu-dialog { background: #2b2b2b; color: #eee; } .hsu-notes { background: #1e1e1e; border-color: #444; } .hsu-progress-bar { background: #1e1e1e; } .hsu-btn-secondary { color: #eee; border-color: #555; } .hsu-btn-secondary:hover:not(:disabled) { background: #3a3a3a; } }
      .hsu-dialog h3 { margin: 0 0 4px; font-size: 16px; font-weight: 600; }
      .hsu-current { color: #888; font-size: 12px; margin-bottom: 14px; }
      .hsu-notes { max-height: 220px; overflow-y: auto; background: #f5f5f5; border: 1px solid #ddd; border-radius: 6px; padding: 10px 12px; line-height: 1.6; margin-bottom: 14px; white-space: pre-wrap; }
      .hsu-progress { margin-bottom: 14px; display: none; }
      .hsu-progress.visible { display: block; }
      .hsu-progress-bar { width: 100%; height: 6px; background: #eee; border-radius: 3px; overflow: hidden; margin-bottom: 6px; }
      .hsu-progress-fill { height: 100%; width: 0%; background: #007aff; transition: width 0.2s ease; }
      .hsu-progress-text { font-size: 12px; color: #888; }
      .hsu-actions { display: flex; gap: 8px; justify-content: flex-end; }
      .hsu-btn-primary, .hsu-btn-secondary { padding: 7px 14px; border-radius: 6px; cursor: pointer; font-size: 13px; font-family: inherit; border: 1px solid #ccc; }
      .hsu-btn-primary { background: #007aff; color: #fff; border-color: #007aff; }
      .hsu-btn-primary:hover:not(:disabled) { filter: brightness(1.1); }
      .hsu-btn-secondary { background: transparent; }
      .hsu-btn-secondary:hover:not(:disabled) { background: #f0f0f0; }
      .hsu-btn-primary:disabled, .hsu-btn-secondary:disabled { opacity: 0.5; cursor: not-allowed; }
    `;
      const style = document.createElement('style');
      style.textContent = css;
      document.head.appendChild(style);

      const overlay = document.createElement('div');
      overlay.className = 'hsu-overlay';
      overlay.innerHTML = `
      <div class="hsu-dialog">
        <h3>发现新版本 v${escapeHtml(info.version)}</h3>
        <div class="hsu-current">当前版本 v${escapeHtml(info.currentVersion || '')}</div>
        <div class="hsu-notes">${formatNotes(info.body)}</div>
        <div class="hsu-progress">
          <div class="hsu-progress-bar"><div class="hsu-progress-fill"></div></div>
          <div class="hsu-progress-text">准备下载...</div>
        </div>
        <div class="hsu-actions">
          <button class="hsu-btn-secondary" data-act="skip">跳过此版本</button>
          <button class="hsu-btn-secondary" data-act="later">稍后提醒</button>
          <button class="hsu-btn-primary" data-act="install">立即更新并重启</button>
        </div>
      </div>`;
      document.body.appendChild(overlay);

      const close = () => { overlay.remove(); style.remove(); };
      const $ = (sel) => overlay.querySelector(sel);
      const skipBtn = $('[data-act="skip"]');
      const laterBtn = $('[data-act="later"]');
      const installBtn = $('[data-act="install"]');
      const progressEl = $('.hsu-progress');
      const fillEl = $('.hsu-progress-fill');
      const textEl = $('.hsu-progress-text');

      skipBtn.onclick = () => { localStorage.setItem(DISMISS_KEY, info.version); close(); };
      laterBtn.onclick = close;
      installBtn.onclick = () => {
        skipBtn.disabled = laterBtn.disabled = installBtn.disabled = true;
        progressEl.classList.add('visible');
        runInstall({ fillEl, textEl, rid: info.rid }).then(close).catch((err) => {
          textEl.textContent = '更新失败: ' + (err && err.message ? err.message : String(err));
          skipBtn.disabled = laterBtn.disabled = installBtn.disabled = false;
        });
      };
    }

    async function runInstall({ fillEl, textEl, rid }) {
      let downloaded = 0;
      let total = 0;
      const channel = new Channel();
      channel.onmessage = (event) => {
        if (event.event === 'Started') {
          total = event.data.contentLength || 0;
          textEl.textContent = total > 0 ? `开始下载 ${formatSize(total)}...` : '开始下载...';
        } else if (event.event === 'Progress') {
          downloaded += event.data.chunkLength || 0;
          if (total > 0) {
            const pct = Math.min(100, (downloaded / total) * 100);
            fillEl.style.width = pct + '%';
            textEl.textContent = `下载中 ${formatSize(downloaded)} / ${formatSize(total)} (${pct.toFixed(0)}%)`;
          } else {
            textEl.textContent = `已下载 ${formatSize(downloaded)}`;
          }
        } else if (event.event === 'Finished') {
          fillEl.style.width = '100%';
          textEl.textContent = '下载完成，正在安装...';
        }
      };
      await invoke('plugin:updater|download_and_install', { rid, onEvent: channel });
      textEl.textContent = '安装完成，即将重启...';
      await invoke('plugin:process|restart');
    }

    function escapeHtml(s) {
      return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
    }
    function formatNotes(body) {
      if (!body) return '<span style="color:#999;font-style:italic;">（无更新说明）</span>';
      return escapeHtml(body).replace(/\n/g, '<br>');
    }
    function formatSize(b) {
      if (b < 1024) return b + ' B';
      if (b < 1024 * 1024) return (b / 1024).toFixed(1) + ' KB';
      return (b / 1024 / 1024).toFixed(2) + ' MB';
    }

    // 启动 5 秒后静默检查(有新版才弹窗;点过「跳过此版本」的不再打扰)
    const scheduleSilentCheck = () => { setTimeout(() => { checkForUpdates({ silent: true }); }, 5000); };
    if (document.readyState === 'complete') scheduleSilentCheck();
    else window.addEventListener('load', scheduleSilentCheck, { once: true });

    return true;
  }

  if (!init()) document.addEventListener('DOMContentLoaded', init, { once: true });
})();
