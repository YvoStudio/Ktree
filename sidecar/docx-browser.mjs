import { renderAsync } from 'docx-preview';

// 在隔离的预览 frame 中渲染原件；资源使用 data URL,关闭弹窗即可释放。
window.KtreeDocx = {
  async render(bytes, body, styles) {
    await renderAsync(bytes, body, styles, {
      className: 'ktree-docx',
      inWrapper: true,
      breakPages: true,
      ignoreLastRenderedPageBreak: false,
      renderHeaders: true,
      renderFooters: true,
      renderFootnotes: true,
      renderEndnotes: true,
      useBase64URL: true,
      renderAltChunks: false,
    });
  },
};
