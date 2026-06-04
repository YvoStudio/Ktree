#!/usr/bin/env node
// Ktree 飞书同步 sidecar(云文档绑定)
//
// 协议:从 stdin 读一行 JSON,向 stdout 写一行 JSON。
//   入:  { "app_id": "...", "app_secret": "...",
//          "target_type": "folder" | "doc", "target_token": "...",
//          "kb_root": "/abs/path", "dest_prefix": "cloud/feishu/<绑定名>",
//          "mode": "full" | "sync" }
//   出成功:{ "ok": true,
//            "documents": [ { "rel_path", "title" } ],   // 本轮有变化、需要 ingest 的
//            "present": [ "rel_path", ... ],              // 同步后实际存在的全部文档
//            "skipped": 3, "errors": [ { "doc", "error" } ] }
//   出失败:{ "ok": false, "error": "..." }
//
// 行为:
//   - target_type=folder:递归遍历飞书共享文件夹,把每个文档(docx / bitable)转成 Markdown
//   - target_type=doc:只同步这一篇文档(docx 或 bitable,自动识别)
//   - md 写入 <kb_root>/src/<dest_prefix>/<层级>/<文档名>.md(rel_path 相对 src/)
//   - 图片/附件写入 <kb_root>/docs/<dest_prefix>/<层级>/<文档名>.assets/,
//     md 内用同目录相对路径 `<文档名>.assets/xxx.png` 引用
//   - 严格镜像:src/<dest_prefix>/ 下不属于本轮飞书内容的文件一律删除
//
// 日志全部走 stderr,stdout 只输出最后一行 JSON。
//
// 由 chibo-kb/scripts/feishu.py 翻译改造而来,忠实保留 docx blocks→md 的
// 各种 block_type 处理、嵌套表格、bitable→md 表格、画板思维导图、图片/文件下载。

const fs = require('fs');
const path = require('path');

const API_BASE = 'https://open.feishu.cn/open-apis';

function log(...args) {
  console.error(...args);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ==========================================================
//  飞书 API 客户端
// ==========================================================

class FeishuClient {
  constructor(appId, appSecret) {
    this.appId = appId;
    this.appSecret = appSecret;
    this._token = null;
    this._tokenExpire = 0;
  }

  async _getToken() {
    const now = Date.now() / 1000;
    if (this._token && now < this._tokenExpire - 60) {
      return this._token;
    }
    const resp = await fetch(`${API_BASE}/auth/v3/tenant_access_token/internal`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ app_id: this.appId, app_secret: this.appSecret }),
    });
    const data = await resp.json();
    if (data.code !== 0) {
      throw new Error(`获取 token 失败: ${data.msg}`);
    }
    this._token = data.tenant_access_token;
    this._tokenExpire = Date.now() / 1000 + (data.expire || 7200);
    return this._token;
  }

  // 统一请求:自动鉴权、限流重试、token 过期重取
  async _request(method, urlPath, { params, body, raw } = {}) {
    let url = `${API_BASE}${urlPath}`;
    if (params) {
      const qs = new URLSearchParams(params).toString();
      if (qs) url += `?${qs}`;
    }
    for (let attempt = 0; attempt < 4; attempt++) {
      const headers = { Authorization: `Bearer ${await this._getToken()}` };
      const init = { method, headers };
      if (body !== undefined) {
        headers['Content-Type'] = 'application/json';
        init.body = JSON.stringify(body);
      }
      const resp = await fetch(url, init);
      if (resp.status === 429) {
        const wait = 2 ** attempt;
        log(`  [限流] 等待 ${wait}s 后重试...`);
        await sleep(wait * 1000);
        continue;
      }
      const contentType = resp.headers.get('content-type') || '';
      if (resp.status === 200 && contentType.startsWith('application/json')) {
        const data = await resp.json();
        const code = data.code || 0;
        // token 过期
        if (code === 99991663 || code === 99991664) {
          this._token = null;
          continue;
        }
        if (code !== 0) {
          throw new Error(`API 错误 [${urlPath}]: code=${code}, msg=${data.msg}`);
        }
        return data;
      }
      if (raw) {
        // 二进制下载流
        return resp;
      }
      // 非 JSON 的 200 / 其它状态码,尝试解析或抛错
      if (resp.status === 200) {
        try {
          return await resp.json();
        } catch (e) {
          return resp;
        }
      }
      throw new Error(`HTTP ${resp.status} [${urlPath}]`);
    }
    throw new Error(`请求失败(重试已用尽): ${urlPath}`);
  }

  async _jsonRequest(method, urlPath, opts) {
    return this._request(method, urlPath, opts);
  }

  // 自动分页,返回所有数据
  async _paginatedGet(urlPath, params, dataKey) {
    const items = [];
    let pageToken = null;
    while (true) {
      const p = { ...params };
      if (pageToken) p.page_token = pageToken;
      const resp = await this._jsonRequest('GET', urlPath, { params: p });
      const data = resp.data || {};
      const chunk = data[dataKey] || [];
      for (const it of chunk) items.push(it);
      if (!data.has_more) break;
      pageToken = data.page_token;
      await sleep(100);
    }
    return items;
  }

  // ----- 云盘 -----

  async listFolder(folderToken) {
    return this._paginatedGet(
      '/drive/v1/files',
      { folder_token: folderToken, page_size: '200' },
      'files'
    );
  }

  // ----- 文档 -----

  async getDocumentBlocks(documentId) {
    return this._paginatedGet(
      `/docx/v1/documents/${documentId}/blocks`,
      { page_size: '500' },
      'items'
    );
  }

  async getDocumentInfo(documentId) {
    const resp = await this._jsonRequest('GET', `/docx/v1/documents/${documentId}`);
    return resp.data;
  }

  // ----- 多维表格 -----

  async getBitableTables(appToken) {
    return this._paginatedGet(
      `/bitable/v1/apps/${appToken}/tables`,
      { page_size: '100' },
      'items'
    );
  }

  async getBitableFields(appToken, tableId) {
    return this._paginatedGet(
      `/bitable/v1/apps/${appToken}/tables/${tableId}/fields`,
      { page_size: '100' },
      'items'
    );
  }

  async getBitableRecords(appToken, tableId) {
    return this._paginatedGet(
      `/bitable/v1/apps/${appToken}/tables/${tableId}/records`,
      { page_size: '500' },
      'items'
    );
  }

  // ----- 媒体下载 -----

  async downloadMedia(fileToken, savePath) {
    const resp = await this._request('GET', `/drive/v1/medias/${fileToken}/download`, {
      raw: true,
    });
    if (!resp.ok && resp.status !== 200) {
      throw new Error(`下载失败 HTTP ${resp.status}`);
    }
    fs.mkdirSync(path.dirname(savePath), { recursive: true });
    const buf = Buffer.from(await resp.arrayBuffer());
    fs.writeFileSync(savePath, buf);
    await sleep(200);
  }
}

// ==========================================================
//  文件夹遍历
// ==========================================================

function cleanName(name) {
  let n = name || '';
  for (const ch of '/\\:*?"<>|') {
    n = n.split(ch).join('_');
  }
  return n.trim();
}

// 递归遍历飞书文件夹,返回扁平文件列表
async function scanFeishuFolder(client, folderToken, parentPath = '') {
  const files = await client.listFolder(folderToken);
  const result = [];

  for (const f of files) {
    const name = cleanName(f.name || '');
    const ftype = f.type || '';
    const token = f.token || '';
    const modified = f.modified_time || '';

    if (ftype === 'folder') {
      const subPath = parentPath ? `${parentPath}/${name}` : name;
      const sub = await scanFeishuFolder(client, token, subPath);
      for (const s of sub) result.push(s);
    } else if (ftype === 'shortcut') {
      // 快捷方式:解析实际目标
      const shortcutInfo = f.shortcut_info || {};
      const targetToken = shortcutInfo.target_token || '';
      const targetType = shortcutInfo.target_type || '';
      if (targetToken && (targetType === 'docx' || targetType === 'bitable')) {
        result.push({
          token: targetToken,
          name,
          type: targetType,
          modified_time: modified,
          relative_path: parentPath,
        });
      }
    } else if (ftype === 'docx' || ftype === 'bitable') {
      result.push({
        token,
        name,
        type: ftype,
        modified_time: modified,
        relative_path: parentPath,
      });
    }
  }
  return result;
}

// ==========================================================
//  文档 Blocks → Markdown
// ==========================================================

// block_type → 内容 key 的映射
const BLOCK_CONTENT_KEY = {
  2: 'text',
  3: 'heading1', 4: 'heading2', 5: 'heading3',
  6: 'heading4', 7: 'heading5', 8: 'heading6', 9: 'heading7',
  10: 'heading8', 11: 'heading9',
  12: 'ordered', 13: 'bullet',
  14: 'code', 15: 'quote',
};

// 将飞书 text_elements 数组转换为 Markdown 文本
function parseTextElements(elements) {
  const parts = [];
  for (const elem of elements || []) {
    const tr = elem.text_run;
    if (!tr) {
      // mention_doc 等其他类型,取 text 降级
      const mention = elem.mention_doc || elem.mention_user;
      if (mention) {
        parts.push(mention.title || '');
      }
      const equation = elem.equation;
      if (equation) {
        parts.push(`$${equation.content || ''}$`);
      }
      continue;
    }

    let text = tr.content || '';
    if (!text) continue;

    const style = tr.text_element_style || {};
    const link = (style.link && style.link.url) || '';

    if (style.code_inline) {
      text = `\`${text}\``;
    } else {
      if (style.bold && style.italic) {
        text = `***${text}***`;
      } else if (style.bold) {
        text = `**${text}**`;
      } else if (style.italic) {
        text = `*${text}*`;
      }
      if (style.strikethrough) {
        text = `~~${text}~~`;
      }
    }

    if (link) {
      // URL 解码
      let decoded = link;
      try {
        decoded = decodeURIComponent(link);
      } catch (e) {
        decoded = link;
      }
      text = `[${text}](${decoded})`;
    }

    parts.push(text);
  }
  return parts.join('');
}

// 从 block 中提取文本内容
function getBlockText(block) {
  const bt = block.block_type || 0;
  const contentKey = BLOCK_CONTENT_KEY[bt];
  let elements = null;
  if (contentKey) {
    const content = block[contentKey] || {};
    elements = content.elements;
  }
  // 飞书 API 的 content key 可能与 block_type 不一致(如 bullet 内容在 ordered 下)
  // 若映射 key 中没有 elements,遍历 block 其他字段查找
  if (!elements) {
    for (const [k, v] of Object.entries(block)) {
      if (k === 'block_id' || k === 'block_type' || k === 'parent_id' || k === 'children') {
        continue;
      }
      if (v && typeof v === 'object' && !Array.isArray(v) && v.elements) {
        elements = v.elements;
        break;
      }
    }
  }
  return parseTextElements(elements || []);
}

// 将飞书文档 blocks 转换为 Markdown
async function blocksToMarkdown(blocks, client, assetsDir, assetsDirName) {
  const lines = [];
  const imgCounter = { value: 0 };

  // 构建 block_id → block 的映射
  const blockMap = {};
  for (const b of blocks) {
    blockMap[b.block_id || ''] = b;
  }

  function padNum(n) {
    return String(n).padStart(3, '0');
  }

  function lastNonEmpty() {
    return lines.length > 0 && lines[lines.length - 1].trim();
  }

  // ---- 单元格复杂度判断 ----
  function cellHasComplex(cellBlock, bmap) {
    for (const subId of cellBlock.children || []) {
      const b = bmap[subId] || {};
      const bt = b.block_type || 0;
      if (bt === 12 || bt === 13 || bt === 22 || bt === 27 || bt === 33) {
        return true;
      }
      if (b.children && b.children.length) {
        return true;
      }
    }
    return false;
  }

  // ---- 单元格 → HTML 渲染 ----
  async function renderCellHtml(cellBlock, bmap) {
    const htmlParts = [];

    async function renderBlock(blockId) {
      const b = bmap[blockId] || {};
      const bt = b.block_type || 0;

      // 图片
      if (bt === 22 || bt === 27) {
        const img = b.image || {};
        const fileToken = img.token || '';
        if (!fileToken) return '';
        imgCounter.value += 1;
        const outName = `img_${padNum(imgCounter.value)}.png`;
        const savePath = path.join(assetsDir, outName);
        try {
          fs.mkdirSync(assetsDir, { recursive: true });
          await client.downloadMedia(fileToken, savePath);
          const origW = img.width || 0;
          const scale = img.scale || 1;
          const displayW = origW && scale ? Math.floor(origW * scale) : origW;
          return `<img src="${assetsDirName}/${outName}" width="${displayW}" />`;
        } catch (e) {
          return '';
        }
      }

      // 视频/文件容器 (view → file)
      if (bt === 33) {
        for (const cid of b.children || []) {
          const cb = bmap[cid] || {};
          const fileData = cb.file;
          if (fileData && fileData.token) {
            const fname = fileData.name || 'file';
            const ftoken = fileData.token;
            try {
              fs.mkdirSync(assetsDir, { recursive: true });
              await client.downloadMedia(ftoken, path.join(assetsDir, fname));
              if (fs.statSync(path.join(assetsDir, fname)).size > 0) {
                const videoExts = ['.mp4', '.mov', '.webm', '.avi', '.mkv'];
                if (videoExts.some((e) => fname.toLowerCase().endsWith(e))) {
                  return `<video src="${assetsDirName}/${fname}" controls width="600">${fname}</video>`;
                }
                return `<a href="${assetsDirName}/${fname}">${fname}</a>`;
              }
            } catch (e) {
              // ignore
            }
            return fname;
          }
        }
        return '';
      }

      // 列表项
      if (bt === 12 || bt === 13) {
        const text = getBlockText(b);
        let childHtml = '';
        const childIds = b.children || [];
        if (childIds.length) {
          // 判断子列表类型
          const firstChildBt = (bmap[childIds[0]] || {}).block_type || 0;
          const tag = firstChildBt === 12 ? 'ol' : 'ul';
          const items = [];
          for (const cid of childIds) {
            items.push(`<li>${await renderBlock(cid)}</li>`);
          }
          childHtml = `<${tag}>${items.join('')}</${tag}>`;
        }
        return text + childHtml;
      }

      // 普通文本
      return getBlockText(b);
    }

    // 遍历单元格的直接 children
    const childIds = cellBlock.children || [];
    let i = 0;
    while (i < childIds.length) {
      const b = bmap[childIds[i]] || {};
      const bt = b.block_type || 0;

      if (bt === 12 || bt === 13) {
        // 聚合连续同类型列表项
        const tag = bt === 12 ? 'ol' : 'ul';
        const listItems = [];
        while (i < childIds.length) {
          const lb = bmap[childIds[i]] || {};
          const lbt = lb.block_type || 0;
          if (lbt !== 12 && lbt !== 13) break;
          listItems.push(`<li>${await renderBlock(childIds[i])}</li>`);
          i += 1;
        }
        htmlParts.push(`<${tag}>${listItems.join('')}</${tag}>`);
      } else {
        const rendered = await renderBlock(childIds[i]);
        if (rendered) htmlParts.push(rendered);
        i += 1;
      }
    }

    return htmlParts.join('');
  }

  // ---- 单元格纯文本提取(简单表格用) ----
  function extractCellText(cellBlock, bmap) {
    const parts = [];
    function collect(blockId) {
      const b = bmap[blockId] || {};
      const bt = b.block_type || 0;
      if (bt === 22 || bt === 27) return;
      const text = getBlockText(b);
      if (text) parts.push(text);
      for (const childId of b.children || []) {
        collect(childId);
      }
    }
    for (const subId of cellBlock.children || []) {
      collect(subId);
    }
    return parts.join(' ').split('|').join('\\|').split('\n').join(' ');
  }

  // ---- 表格处理:简单表格用 Markdown,复杂表格用 HTML ----
  async function processTable(tableBlock, bmap) {
    const tableData = tableBlock.table || {};
    const prop = tableData.property || {};
    const rowSize = prop.row_size || 0;
    const colSize = prop.column_size || 0;
    const mergeInfo = prop.merge_info || [];
    const children = tableBlock.children || [];

    // 收集所有单元格 ID(按行分组)
    const rowsCellIds = [];
    const firstChild = children.length ? bmap[children[0]] || {} : {};
    if (firstChild.block_type === 32) {
      for (let i = 0; i < children.length; i += colSize) {
        rowsCellIds.push(children.slice(i, i + colSize));
      }
    } else {
      for (const rowId of children) {
        const rowBlock = bmap[rowId] || {};
        rowsCellIds.push(rowBlock.children || []);
      }
    }

    if (!rowsCellIds.length) return;

    // 解析合并信息:构建被覆盖的单元格集合
    let hasMerge = false;
    const covered = new Set(); // "row,col" 被其他单元格的 span 覆盖
    const mergeMap = {}; // "row,col" -> [rowSpan, colSpan]
    if (mergeInfo && mergeInfo.length === rowSize * colSize) {
      for (let idx = 0; idx < mergeInfo.length; idx++) {
        const mi = mergeInfo[idx];
        const r = Math.floor(idx / colSize);
        const c = idx % colSize;
        const rs = mi.row_span || 1;
        const cs = mi.col_span || 1;
        if (rs > 1 || cs > 1) {
          hasMerge = true;
          mergeMap[`${r},${c}`] = [rs, cs];
          for (let dr = 0; dr < rs; dr++) {
            for (let dc = 0; dc < cs; dc++) {
              if (dr === 0 && dc === 0) continue;
              covered.add(`${r + dr},${c + dc}`);
            }
          }
        }
      }
    }

    // 检测是否有复杂单元格
    let hasComplex = false;
    for (const rowIds of rowsCellIds) {
      for (const cellId of rowIds) {
        const cellBlock = bmap[cellId] || {};
        if (cellHasComplex(cellBlock, bmap)) {
          hasComplex = true;
          break;
        }
      }
      if (hasComplex) break;
    }

    if (lastNonEmpty()) lines.push('');

    if (hasComplex || hasMerge) {
      // HTML 表格
      lines.push('<table>');
      for (let rowIdx = 0; rowIdx < rowsCellIds.length; rowIdx++) {
        const rowIds = rowsCellIds[rowIdx];
        const tag = rowIdx === 0 ? 'th' : 'td';
        lines.push('<tr>');
        for (let colIdx = 0; colIdx < rowIds.length; colIdx++) {
          if (covered.has(`${rowIdx},${colIdx}`)) continue;
          const cellBlock = bmap[rowIds[colIdx]] || {};
          const html = await renderCellHtml(cellBlock, bmap);
          let spanAttr = '';
          if (mergeMap[`${rowIdx},${colIdx}`]) {
            const [rs, cs] = mergeMap[`${rowIdx},${colIdx}`];
            if (rs > 1) spanAttr += ` rowspan="${rs}"`;
            if (cs > 1) spanAttr += ` colspan="${cs}"`;
          }
          lines.push(`<${tag}${spanAttr}>${html}</${tag}>`);
        }
        lines.push('</tr>');
      }
      lines.push('</table>');
    } else {
      // Markdown 表格
      const rowsText = [];
      for (const rowIds of rowsCellIds) {
        const cells = [];
        for (const cellId of rowIds) {
          const cellBlock = bmap[cellId] || {};
          cells.push(extractCellText(cellBlock, bmap));
        }
        while (cells.length < colSize) cells.push('');
        rowsText.push(cells);
      }
      lines.push('| ' + rowsText[0].join(' | ') + ' |');
      lines.push('| ' + new Array(rowsText[0].length).fill('---').join(' | ') + ' |');
      for (let i = 1; i < rowsText.length; i++) {
        lines.push('| ' + rowsText[i].join(' | ') + ' |');
      }
    }
    lines.push('');
  }

  // ---- 图片 block 处理 ----
  async function processImage(block) {
    const imgData = block.image || {};
    const fileToken = imgData.token || '';
    if (!fileToken) return;
    imgCounter.value += 1;
    const outName = `img_${padNum(imgCounter.value)}.png`;
    const savePath = path.join(assetsDir, outName);
    try {
      fs.mkdirSync(assetsDir, { recursive: true });
      await client.downloadMedia(fileToken, savePath);
      // 确保图片前有空行
      if (lastNonEmpty()) lines.push('');
      // 根据飞书 scale 计算实际显示宽度,始终设置 width 保证还原源文档排版
      const origW = imgData.width || 0;
      const scale = imgData.scale || 1;
      const displayW = origW && scale ? Math.floor(origW * scale) : origW;
      if (displayW) {
        lines.push(`<img src="${assetsDirName}/${outName}" width="${displayW}" />`);
      } else {
        lines.push(`![图片](${assetsDirName}/${outName})`);
      }
      lines.push('');
    } catch (e) {
      log(`    [警告] 图片下载失败 (${fileToken}): ${e.message || e}`);
    }
  }

  // ---- 文件附件 block 处理(视频、文档等) ----
  async function processFile(block) {
    const fileData = block.file || {};
    const fileToken = fileData.token || '';
    const fileName = fileData.name || 'file';
    if (!fileToken) return;
    const savePath = path.join(assetsDir, fileName);
    try {
      fs.mkdirSync(assetsDir, { recursive: true });
      await client.downloadMedia(fileToken, savePath);
      if (fs.statSync(savePath).size === 0) {
        try {
          fs.unlinkSync(savePath);
        } catch (e) {
          // ignore
        }
        lines.push(`[${fileName}](文件下载失败)`);
        lines.push('');
        return;
      }
      if (lastNonEmpty()) lines.push('');
      const videoExts = ['.mp4', '.mov', '.webm', '.avi', '.mkv'];
      if (videoExts.some((e) => fileName.toLowerCase().endsWith(e))) {
        lines.push(`<video src="${assetsDirName}/${fileName}" controls width="600">${fileName}</video>`);
      } else {
        lines.push(`[${fileName}](${assetsDirName}/${fileName})`);
      }
      lines.push('');
    } catch (e) {
      log(`    [警告] 文件下载失败 (${fileName}): ${e.message || e}`);
      lines.push(`[${fileName}](文件下载失败)`);
      lines.push('');
    }
  }

  // ---- 画板 block 处理 (bt=43):提取思维导图、内部图片和文本标注 ----
  async function processBoard(block) {
    const boardData = block.board || {};
    const boardToken = boardData.token || '';
    if (!boardToken) return;

    let nodes;
    try {
      const resp = await client._jsonRequest(
        'GET',
        `/board/v1/whiteboards/${boardToken}/nodes`
      );
      nodes = (resp.data && resp.data.nodes) || [];
    } catch (e) {
      log(`    [警告] 画板节点获取失败 (${boardToken}): ${e.message || e}`);
      return;
    }

    if (!nodes.length) return;

    // ---------- 思维导图节点 ----------
    const mmNodes = {};
    const mmRoots = [];
    for (const n of nodes) {
      if (n.type !== 'mind_map') continue;
      const nid = n.id;
      const text = (n.text && n.text.text) || '';
      const parentId = (n.mind_map && n.mind_map.parent_id) || '';
      mmNodes[nid] = {
        text,
        parent_id: parentId,
        children: [],
        y: n.y || 0,
      };
      if ('mind_map_root' in n || !parentId) {
        mmRoots.push(nid);
      }
    }
    // 构建 children
    for (const [nid, info] of Object.entries(mmNodes)) {
      const pid = info.parent_id;
      if (pid && mmNodes[pid]) {
        mmNodes[pid].children.push(nid);
      }
    }
    // 按 y 坐标排序
    for (const info of Object.values(mmNodes)) {
      info.children.sort((a, b) => mmNodes[a].y - mmNodes[b].y);
    }

    if (Object.keys(mmNodes).length) {
      if (lastNonEmpty()) lines.push('');
      const renderMm = (nid, depth = 0) => {
        const info = mmNodes[nid];
        const text = info.text;
        if (!text && !info.children.length) return;
        if (depth === 0) {
          lines.push(`## ${text}`);
          lines.push('');
        } else {
          const indent = '  '.repeat(depth - 1);
          lines.push(`${indent}- ${text}`);
        }
        for (const cid of info.children) {
          renderMm(cid, depth + 1);
        }
        if (depth === 0) lines.push('');
      };
      for (const rid of mmRoots) {
        renderMm(rid);
      }
    }

    // ---------- 分离图片和文本节点 ----------
    const images = [];
    const texts = [];
    for (const n of nodes) {
      const ntype = n.type || '';
      if (ntype === 'image' && n.image && n.image.token) {
        images.push(n);
      } else if (ntype === 'text_shape' && n.text && n.text.text) {
        texts.push(n);
      }
    }

    if (!images.length && !texts.length) return;

    if (lastNonEmpty()) lines.push('');

    // 按 y 分行(间距小于 80 归为同行),行内按 x 排序
    images.sort((a, b) => (a.y || 0) - (b.y || 0));
    const rows = [];
    for (const img of images) {
      const iy = img.y || 0;
      let placed = false;
      for (const row of rows) {
        if (Math.abs(iy - (row[0].y || 0)) < 80) {
          row.push(img);
          placed = true;
          break;
        }
      }
      if (!placed) rows.push([img]);
    }
    // 行从上到下,行内按 x 从左到右
    rows.sort((a, b) => (a[0].y || 0) - (b[0].y || 0));
    const orderedImages = [];
    for (const row of rows) {
      row.sort((a, b) => (a.x || 0) - (b.x || 0));
      for (const img of row) orderedImages.push(img);
    }

    // 为每张图片找上方最近的文本标注作标题
    const findLabel = (imgNode) => {
      const ix = imgNode.x || 0;
      const iy = imgNode.y || 0;
      const iw = imgNode.width || 0;
      const imgCx = ix + iw / 2;
      let best = null;
      let bestDist = Infinity;
      for (const t of texts) {
        const tx = t.x || 0;
        const ty = t.y || 0;
        const tw = t.width || 0;
        const textCx = tx + tw / 2;
        // 文本应在图片上方(ty < iy),且水平中心对齐
        if (ty >= iy) continue;
        const hDist = Math.abs(textCx - imgCx);
        const vDist = iy - ty;
        if (hDist < iw && vDist < 200) {
          const dist = hDist + vDist;
          if (dist < bestDist) {
            bestDist = dist;
            best = t;
          }
        }
      }
      if (best) return (best.text && best.text.text) || '';
      return '';
    };

    const usedLabels = new Set();
    for (const imgNode of orderedImages) {
      const imgToken = imgNode.image.token;
      imgCounter.value += 1;
      const outName = `img_${padNum(imgCounter.value)}.png`;
      const savePath = path.join(assetsDir, outName);
      try {
        fs.mkdirSync(assetsDir, { recursive: true });
        await client.downloadMedia(imgToken, savePath);
        if (fs.statSync(savePath).size === 0) {
          try {
            fs.unlinkSync(savePath);
          } catch (e) {
            // ignore
          }
          imgCounter.value -= 1;
          continue;
        }
        const displayW = Math.floor(imgNode.width || 0) || null;
        const label = findLabel(imgNode);
        if (label && !usedLabels.has(label)) {
          usedLabels.add(label);
          lines.push(`**${label}**`);
          lines.push('');
        }
        if (displayW) {
          lines.push(`<img src="${assetsDirName}/${outName}" width="${displayW}" />`);
        } else {
          lines.push(`![图片](${assetsDirName}/${outName})`);
        }
        lines.push('');
      } catch (e) {
        log(`    [警告] 画板图片下载失败 (${imgToken}): ${e.message || e}`);
        imgCounter.value -= 1;
      }
    }

    // 输出未关联到图片的独立文本
    const standaloneTexts = texts.filter(
      (t) => !usedLabels.has((t.text && t.text.text) || '')
    );
    if (standaloneTexts.length) {
      standaloneTexts.sort((a, b) => {
        const dy = (a.y || 0) - (b.y || 0);
        if (dy !== 0) return dy;
        return (a.x || 0) - (b.x || 0);
      });
      for (const t of standaloneTexts) {
        const txt = (t.text && t.text.text) || '';
        if (txt) {
          lines.push(txt);
          lines.push('');
        }
      }
    }
  }

  // ---- 文档内嵌入的多维表格 (bt=53 reference_base) ----
  async function processEmbeddedBitable(block) {
    const ref = block.reference_base || {};
    const tokenRaw = ref.token || '';
    const viewId = ref.view_id || '';
    if (!tokenRaw) return;
    // token 格式: app_token_table_id 或 app_token
    const splitIdx = tokenRaw.indexOf('_');
    const appToken = splitIdx >= 0 ? tokenRaw.slice(0, splitIdx) : tokenRaw;
    const tableId = splitIdx >= 0 ? tokenRaw.slice(splitIdx + 1) : null;

    try {
      let tablesToConvert;
      if (tableId) {
        tablesToConvert = [{ table_id: tableId }];
      } else {
        tablesToConvert = await client.getBitableTables(appToken);
      }

      for (const tbl of tablesToConvert) {
        const tid = tbl.table_id || tableId;
        const tname = tbl.name || '';

        const fields = await client.getBitableFields(appToken, tid);
        const records = await client.getBitableRecords(appToken, tid);

        if (!fields.length || !records.length) continue;

        // 读取视图配置:隐藏字段、层级
        let hiddenFieldIds = new Set();
        let hierarchyFieldId = null;
        if (viewId) {
          try {
            const vdata = await client._jsonRequest(
              'GET',
              `/bitable/v1/apps/${appToken}/tables/${tid}/views/${viewId}`
            );
            const vprop =
              (vdata.data && vdata.data.view && vdata.data.view.property) || {};
            hiddenFieldIds = new Set(vprop.hidden_fields || []);
            const hcfg = vprop.hierarchy_config || {};
            hierarchyFieldId = hcfg.field_id;
          } catch (e) {
            // ignore
          }
        }

        // 过滤隐藏字段
        const visibleFields = fields.filter(
          (f) => !hiddenFieldIds.has(f.field_id)
        );
        const fieldNames = visibleFields.map((f) => f.field_name || '');
        // 记录附件字段名(type=17)
        const attachmentFields = new Set(
          visibleFields.filter((f) => f.type === 17).map((f) => f.field_name)
        );
        if (!fieldNames.length) continue;

        // 找层级字段名
        let hierarchyFieldName = null;
        if (hierarchyFieldId) {
          for (const f of fields) {
            if (f.field_id === hierarchyFieldId) {
              hierarchyFieldName = f.field_name;
              break;
            }
          }
        }

        // 渲染 bitable 单元格,附件字段下载图片
        const renderBitableCell = async (fname, val) => {
          if (attachmentFields.has(fname) && Array.isArray(val)) {
            const parts = [];
            for (const item of val) {
              if (!item || typeof item !== 'object') continue;
              const ft = item.file_token || '';
              const fn = item.name || 'file';
              if (!ft) continue;
              imgCounter.value += 1;
              const ext = path.extname(fn) || '.png';
              const outName = `img_${padNum(imgCounter.value)}${ext}`;
              try {
                fs.mkdirSync(assetsDir, { recursive: true });
                await client.downloadMedia(ft, path.join(assetsDir, outName));
                if (fs.statSync(path.join(assetsDir, outName)).size > 0) {
                  parts.push(
                    `<img src="${assetsDirName}/${outName}" width="120" />`
                  );
                } else {
                  try {
                    fs.unlinkSync(path.join(assetsDir, outName));
                  } catch (e) {
                    // ignore
                  }
                  imgCounter.value -= 1;
                }
              } catch (e) {
                imgCounter.value -= 1;
              }
            }
            return parts.join('');
          }
          return bitableCellToStr(val);
        };

        if (lastNonEmpty()) lines.push('');
        if (tname) {
          lines.push(`**${tname}**`);
          lines.push('');
        }

        if (hierarchyFieldName) {
          // 层级展示:构建树 -> 输出带缩进的表格
          const recMap = {};
          for (const r of records) recMap[r.record_id || ''] = r;
          const childrenMap = {};
          const rootRecords = [];

          for (const r of records) {
            const rf = r.fields || {};
            const parent = rf[hierarchyFieldName];
            let pid = null;
            if (parent && Array.isArray(parent) && parent.length) {
              const rids = parent[0].record_ids;
              if (rids && rids.length) pid = rids[0];
            }
            if (pid && recMap[pid]) {
              if (!childrenMap[pid]) childrenMap[pid] = [];
              childrenMap[pid].push(r);
            } else {
              rootRecords.push(r);
            }
          }

          // 层级背景色:根记录深,逐层变浅
          const DEPTH_COLORS = [
            '#e8f4f8', // 根记录 - 浅蓝
            '#f0f8ef', // 一级子 - 浅绿
            '#fdf6ec', // 二级子 - 浅黄
            '#f5f0fa', // 三级子 - 浅紫
          ];

          // HTML 表格
          lines.push('<table>');
          lines.push(
            '<tr>' + fieldNames.map((fn) => `<th>${fn}</th>`).join('') + '</tr>'
          );

          const emitRow = async (rec, depth) => {
            const rf = rec.fields || {};
            const rid = rec.record_id || '';
            const bg = DEPTH_COLORS[Math.min(depth, DEPTH_COLORS.length - 1)];
            const cellsHtml = [];
            for (let i = 0; i < fieldNames.length; i++) {
              const fname = fieldNames[i];
              const val = rf[fname] !== undefined ? rf[fname] : '';
              let text = await renderBitableCell(fname, val);
              if (i === 0) {
                const indent = '&nbsp;&nbsp;&nbsp;&nbsp;'.repeat(depth);
                if (depth === 0 && text) {
                  text = `<b>${text}</b>`;
                }
                text = indent + text;
              }
              cellsHtml.push(`<td>${text}</td>`);
            }
            lines.push(`<tr style="background:${bg}">` + cellsHtml.join('') + '</tr>');
            for (const child of childrenMap[rid] || []) {
              await emitRow(child, depth + 1);
            }
          };

          for (const r of rootRecords) {
            await emitRow(r, 0);
          }

          lines.push('</table>');
        } else if (attachmentFields.size) {
          // 含附件字段,用 HTML 表格
          lines.push('<table>');
          lines.push(
            '<tr>' + fieldNames.map((fn) => `<th>${fn}</th>`).join('') + '</tr>'
          );
          for (const record of records) {
            const rf = record.fields || {};
            const cellsHtml = [];
            for (const fname of fieldNames) {
              const val = rf[fname] !== undefined ? rf[fname] : '';
              cellsHtml.push(`<td>${await renderBitableCell(fname, val)}</td>`);
            }
            lines.push('<tr>' + cellsHtml.join('') + '</tr>');
          }
          lines.push('</table>');
        } else {
          // 纯文本,Markdown 表格
          lines.push('| ' + fieldNames.join(' | ') + ' |');
          lines.push(
            '| ' + new Array(fieldNames.length).fill('---').join(' | ') + ' |'
          );
          for (const record of records) {
            const rf = record.fields || {};
            const cells = [];
            for (const fname of fieldNames) {
              const val = rf[fname] !== undefined ? rf[fname] : '';
              cells.push(bitableCellToStr(val));
            }
            lines.push('| ' + cells.join(' | ') + ' |');
          }
        }

        lines.push('');
      }
    } catch (e) {
      log(`    [警告] 嵌入多维表格转换失败 (${tokenRaw}): ${e.message || e}`);
    }
  }

  // ---- 单个 block 处理 ----
  async function processBlock(block, depth = 0) {
    const bt = block.block_type || 0;

    // 页面根节点 — 处理 children
    if (bt === 1) {
      for (const childId of block.children || []) {
        const child = blockMap[childId];
        if (child) await processBlock(child, depth);
      }
      return;
    }

    // 文本段落
    if (bt === 2) {
      const text = getBlockText(block);
      lines.push(text);
      lines.push('');
      for (const childId of block.children || []) {
        const child = blockMap[childId];
        if (child) await processBlock(child, depth);
      }
      return;
    }

    // 标题 (heading1-heading9 → # ~ #########)
    if (bt >= 3 && bt <= 11) {
      const level = bt - 2;
      const text = getBlockText(block);
      lines.push(`${'#'.repeat(level)} ${text}`);
      lines.push('');
      // 标题可能包含子 block(如折叠内容、嵌套表格等)
      for (const childId of block.children || []) {
        const child = blockMap[childId];
        if (child) await processBlock(child, depth);
      }
      return;
    }

    // 有序列表
    if (bt === 12) {
      const text = getBlockText(block);
      const indent = '  '.repeat(depth);
      lines.push(`${indent}1. ${text}`);
      for (const childId of block.children || []) {
        const child = blockMap[childId];
        if (child) await processBlock(child, depth + 1);
      }
      return;
    }

    // 无序列表
    if (bt === 13) {
      const text = getBlockText(block);
      const indent = '  '.repeat(depth);
      lines.push(`${indent}- ${text}`);
      for (const childId of block.children || []) {
        const child = blockMap[childId];
        if (child) await processBlock(child, depth + 1);
      }
      return;
    }

    // 代码块
    if (bt === 14) {
      const codeData = block.code || {};
      const elements = codeData.elements || [];
      const text = parseTextElements(elements);
      let lang = (codeData.style && codeData.style.language) || '';
      // 飞书 language 是数字枚举,降级不标注
      if (typeof lang === 'number') lang = '';
      lines.push('```' + lang);
      lines.push(text);
      lines.push('```');
      lines.push('');
      return;
    }

    // 引用
    if (bt === 15) {
      const text = getBlockText(block);
      for (const line of text.split('\n')) {
        lines.push(`> ${line}`);
      }
      lines.push('');
      return;
    }

    // 表格 (bt=18 旧版, bt=31 新版)
    if (bt === 18 || bt === 31) {
      await processTable(block, blockMap);
      return;
    }

    // 图片 (bt=22 独立图片, bt=27 嵌入图片/媒体)
    if (bt === 22 || bt === 27) {
      await processImage(block);
      return;
    }

    // grid 多列布局 (bt=24 grid, bt=25 grid_column) — 展开 children
    if (bt === 24 || bt === 25) {
      for (const childId of block.children || []) {
        const child = blockMap[childId];
        if (child) await processBlock(child, depth);
      }
      return;
    }

    // 分割线 / 文件(bt=23 同时用于分割线和文件附件)
    if (bt === 23) {
      const fileData = block.file;
      if (fileData && fileData.token) {
        await processFile(block);
      } else {
        lines.push('---');
        lines.push('');
      }
      return;
    }

    // 视频/文件容器 (view) — 展开子节点
    if (bt === 33) {
      for (const childId of block.children || []) {
        const child = blockMap[childId];
        if (child) await processBlock(child, depth);
      }
      return;
    }

    // 画板 (board) — 提取思维导图、内部图片和文本标注
    if (bt === 43) {
      await processBoard(block);
      return;
    }

    // 嵌入多维表格 (reference_base)
    if (bt === 53) {
      await processEmbeddedBitable(block);
      return;
    }

    // 其他类型(todo、embed 等)降级为文本
    const text = getBlockText(block);
    if (text) {
      lines.push(text);
      lines.push('');
    }
    // 仍处理 children
    for (const childId of block.children || []) {
      const child = blockMap[childId];
      if (child) await processBlock(child, depth);
    }
  }

  // 处理所有顶层 block
  for (const block of blocks) {
    const bt = block.block_type || 0;
    if (bt === 1) {
      // page 根节点,展开 children
      await processBlock(block);
    } else if ((block.parent_id || '') === '') {
      // 无 parent 的顶层 block
      await processBlock(block);
    }
  }

  return lines.join('\n').trim() + '\n';
}

// ==========================================================
//  多维表格 → Markdown
// ==========================================================

// 将多维表格字段值转换为字符串
function bitableCellToStr(val) {
  if (val === null || val === undefined) return '';
  if (typeof val === 'boolean') return val ? '是' : '否';
  if (typeof val === 'string' || typeof val === 'number') {
    return String(val).split('|').join('\\|').split('\n').join(' ');
  }
  if (Array.isArray(val)) {
    // 多选、人员等数组类型
    const parts = [];
    for (const item of val) {
      if (item && typeof item === 'object') {
        // 人员类型
        parts.push(item.name || item.text || JSON.stringify(item));
      } else if (typeof item === 'string') {
        parts.push(item);
      } else {
        parts.push(String(item));
      }
    }
    return parts.join(', ').split('|').join('\\|');
  }
  if (typeof val === 'object') {
    // 单选等
    return (val.text || val.name || JSON.stringify(val)).split('|').join('\\|');
  }
  return String(val).split('|').join('\\|');
}

function nowStamp() {
  const d = new Date();
  const pad = (n) => String(n).padStart(2, '0');
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}`
  );
}

// 将飞书多维表格转换为 Markdown
async function convertBitable(client, appToken, docName) {
  const tables = await client.getBitableTables(appToken);
  const sections = [];

  for (const table of tables) {
    const tableId = table.table_id || '';
    const tableName = table.name || tableId;

    const fields = await client.getBitableFields(appToken, tableId);
    const records = await client.getBitableRecords(appToken, tableId);

    // 字段名列表(按 UI 顺序)
    const fieldNames = fields.map((f) => f.field_name || '');

    const sectionLines = [];
    sectionLines.push(`## ${tableName}`);
    sectionLines.push('');

    if (!fieldNames.length) continue;

    // 表头
    sectionLines.push('| ' + fieldNames.join(' | ') + ' |');
    sectionLines.push(
      '| ' + new Array(fieldNames.length).fill('---').join(' | ') + ' |'
    );

    // 数据行
    for (const record of records) {
      const recordFields = record.fields || {};
      const cells = [];
      for (const fname of fieldNames) {
        const val = recordFields[fname] !== undefined ? recordFields[fname] : '';
        cells.push(bitableCellToStr(val));
      }
      sectionLines.push('| ' + cells.join(' | ') + ' |');
    }

    sections.push(sectionLines.join('\n'));
  }

  const title = `# ${docName}`;
  const meta =
    '> 来源: 飞书多维表格\n' +
    `> 自动生成于: ${nowStamp()}\n` +
    '> 请勿手动编辑此文件';
  return `${title}\n\n${meta}\n\n` + sections.join('\n\n') + '\n';
}

// ==========================================================
//  frontmatter
// ==========================================================

// 从文件名提取标签
function extractTags(stem) {
  let cleaned = stem.replace(/[【\[][^\]】]*[】\]]/g, '').trim();
  cleaned = cleaned.replace(/^项目[-_]?/, '');
  const parts = cleaned.split(/[-_]/);
  return parts.map((p) => p.trim()).filter((p) => p && p.length > 1);
}

// 根据文档名、分类和内容生成 YAML frontmatter
function buildFrontmatter(stem, category, content) {
  // 标题:优先取内容里第一个 # 标题
  let title = stem;
  for (const line of content.split('\n')) {
    const s = line.trim();
    if (s.startsWith('# ')) {
      title = s.slice(2).trim();
      break;
    }
  }

  // 摘要:优先取标题后紧跟的 blockquote,否则取第一段正文
  let summary = '';
  let foundTitle = false;
  for (const line of content.split('\n')) {
    const s = line.trim();
    if (s.startsWith('# ')) {
      foundTitle = true;
      continue;
    }
    if (foundTitle && !s) continue;
    if (foundTitle && s.startsWith('> ')) {
      summary = s.slice(2).trim();
      summary = summary.slice(0, 80) + (summary.length > 80 ? '…' : '');
      break;
    }
    foundTitle = false;
    if (!s || '#>|!-*'.includes(s[0])) continue;
    summary = s.slice(0, 80) + (s.length > 80 ? '…' : '');
    break;
  }

  const tags = extractTags(stem);
  const tagsStr = tags.join(', ');

  const lines = ['---', `title: ${title}`, `category: ${category || '其他'}`];
  if (tagsStr) lines.push(`tags: [${tagsStr}]`);
  if (summary) lines.push(`summary: ${summary}`);
  lines.push('---', '');
  return lines.join('\n');
}

// ==========================================================
//  同步逻辑
// ==========================================================

function toPosix(p) {
  return p.split(path.sep).join('/');
}

// 文档在飞书侧的唯一 key(用于增量状态)
function stateKey(relPath, name) {
  return relPath ? `feishu://${relPath}/${name}` : `feishu://${name}`;
}

// 增量状态文件读写(放在知识库根的 .ktree/ 下,每个绑定一份,按 dest_prefix 区分)
function syncStatePath(kbRoot, destPrefix) {
  const name = destPrefix.split('/').join('_');
  return path.join(kbRoot, '.ktree', `.cloud-sync-${name}.json`);
}
function loadSyncState(kbRoot, destPrefix) {
  const statePath = syncStatePath(kbRoot, destPrefix);
  try {
    if (fs.existsSync(statePath)) {
      return JSON.parse(fs.readFileSync(statePath, 'utf8'));
    }
  } catch (e) {
    log(`[警告] 读取同步状态失败: ${e.message || e}`);
  }
  return {};
}

function saveSyncState(kbRoot, destPrefix, state) {
  const statePath = syncStatePath(kbRoot, destPrefix);
  fs.mkdirSync(path.dirname(statePath), { recursive: true });
  fs.writeFileSync(statePath, JSON.stringify(state, null, 2), 'utf8');
}

// 同步单个文件,返回 { rel_path, title } 或抛错。
// 产出:
//   md       → <kb_root>/src/<dest_prefix>/<层级>/<name>.md
//   图片附件 → <kb_root>/docs/<dest_prefix>/<层级>/<name>.assets/
// md 内用同目录相对路径 `<name>.assets/xxx.png` 引用资源 ——
// ingest 会把 md 软链镜像到 docs/ 同位置,引用恰好命中旁边的 .assets/。
// 不写 frontmatter —— 交给 Ktree 的 ingest 统一加。
async function syncFile(client, fileInfo, kbRoot, destPrefix) {
  const name = fileInfo.name;
  const ftype = fileInfo.type;
  const token = fileInfo.token;
  const relPath = fileInfo.relative_path;

  // md 写到 src/<dest_prefix>/<层级>/<name>.md
  const srcDir = relPath
    ? path.join(kbRoot, 'src', destPrefix, relPath)
    : path.join(kbRoot, 'src', destPrefix);
  fs.mkdirSync(srcDir, { recursive: true });
  const outputPath = path.join(srcDir, `${name}.md`);

  // 图片附件写到 docs/<dest_prefix>/<层级>/<name>.assets/
  const assetsDir = relPath
    ? path.join(kbRoot, 'docs', destPrefix, relPath, `${name}.assets`)
    : path.join(kbRoot, 'docs', destPrefix, `${name}.assets`);
  // md 与 .assets 同目录,引用前缀就是目录名本身
  const assetsRef = `${name}.assets`;

  // rel_path 相对 src/(带 dest_prefix)
  const mdRelPath = relPath
    ? `${destPrefix}/${relPath}/${name}.md`
    : `${destPrefix}/${name}.md`;

  let content;
  if (ftype === 'docx') {
    const blocks = await client.getDocumentBlocks(token);
    const body = await blocksToMarkdown(blocks, client, assetsDir, assetsRef);
    const meta = '> 来源: 飞书文档,由 Ktree 自动同步\n';
    content = `${meta}\n${body}`;
  } else if (ftype === 'bitable') {
    content = await convertBitable(client, token, name);
  } else {
    throw new Error(`不支持的文档类型: ${ftype}`);
  }

  // 没下载到任何资源时清理空的 assets 目录
  try {
    if (fs.existsSync(assetsDir) && fs.readdirSync(assetsDir).length === 0) {
      fs.rmdirSync(assetsDir);
    }
  } catch (e) {
    // ignore
  }

  // 直接写纯 markdown,frontmatter 由 Ktree ingest 统一加
  fs.writeFileSync(outputPath, content, 'utf8');
  log(`  转换完成: ${name} -> src/${mdRelPath}`);

  return { rel_path: mdRelPath, title: name };
}

// 单篇文档模式:自动识别 token 是 docx 还是 bitable,返回文件描述(与文件夹扫描同构)
async function resolveSingleDoc(client, token) {
  // 先按新版文档(docx)取标题
  try {
    const info = await client.getDocumentInfo(token);
    const title = cleanName(
      (info && info.document && info.document.title) || ''
    );
    if (title) {
      return {
        token,
        name: title,
        type: 'docx',
        modified_time: (info && info.document && info.document.revision_id) || '',
        relative_path: '',
      };
    }
  } catch (e) {
    log(`  按 docx 解析失败,尝试多维表格: ${e.message || e}`);
  }
  // 再按多维表格取应用名
  try {
    const resp = await client._jsonRequest('GET', `/bitable/v1/apps/${token}`);
    const title = cleanName((resp.data && resp.data.app && resp.data.app.name) || '');
    if (title) {
      return {
        token,
        name: title,
        type: 'bitable',
        modified_time: (resp.data && resp.data.app && resp.data.app.revision) || '',
        relative_path: '',
      };
    }
  } catch (e) {
    log(`  按 bitable 解析失败: ${e.message || e}`);
  }
  throw new Error('无法识别该文档(既不是新版文档 docx,也不是多维表格)');
}

// 递归列出目录下所有文件(相对 base 的正斜杠路径),跳过隐藏文件
function walkFiles(base, rel = '') {
  const out = [];
  const dir = rel ? path.join(base, rel) : base;
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch (e) {
    return out;
  }
  for (const e of entries) {
    if (e.name.startsWith('.')) continue;
    const r = rel ? `${rel}/${e.name}` : e.name;
    if (e.isDirectory()) {
      out.push(...walkFiles(base, r));
    } else {
      out.push(r);
    }
  }
  return out;
}

// 主流程
async function run(req) {
  const {
    app_id,
    app_secret,
    target_type,
    target_token,
    kb_root,
    dest_prefix,
    mode,
  } = req;
  if (!app_id || !app_secret) throw new Error('缺少 app_id / app_secret');
  if (!target_token) throw new Error('缺少 target_token');
  if (!kb_root) throw new Error('缺少 kb_root');
  if (!dest_prefix) throw new Error('缺少 dest_prefix');
  if (target_type !== 'folder' && target_type !== 'doc') {
    throw new Error(`target_type 必须是 folder 或 doc,收到「${target_type}」`);
  }

  const srcBase = path.join(kb_root, 'src', dest_prefix);
  fs.mkdirSync(srcBase, { recursive: true });
  fs.mkdirSync(path.join(kb_root, '.ktree'), { recursive: true });

  const isSync = mode === 'sync';
  const client = new FeishuClient(app_id, app_secret);

  // 拿到要同步的文件列表(folder:递归扫描;doc:单篇)
  let files;
  if (target_type === 'folder') {
    log('正在扫描飞书文件夹...');
    files = await scanFeishuFolder(client, target_token);
  } else {
    log('正在解析单篇文档...');
    files = [await resolveSingleDoc(client, target_token)];
  }

  const docsCount = files.filter((f) => f.type === 'docx').length;
  const bitableCount = files.filter((f) => f.type === 'bitable').length;
  log(
    `找到 ${files.length} 个文件(文档 ${docsCount},多维表格 ${bitableCount}),开始同步...`
  );

  const state = loadSyncState(kb_root, dest_prefix);
  const documents = [];
  const errors = [];
  let skipped = 0;

  for (const f of files) {
    const key = stateKey(f.relative_path, f.name);
    const existing = state[key];

    // 增量模式:修改时间未变且文件还在盘上则跳过
    if (
      isSync &&
      existing &&
      String(existing.modified_time || '') === String(f.modified_time || '') &&
      existing.rel_path &&
      fs.existsSync(path.join(kb_root, 'src', existing.rel_path))
    ) {
      skipped += 1;
      continue;
    }

    try {
      const doc = await syncFile(client, f, kb_root, dest_prefix);
      documents.push(doc);
      state[key] = {
        token: f.token,
        type: f.type,
        modified_time: f.modified_time,
        rel_path: doc.rel_path,
        converted_at: nowStamp(),
      };
    } catch (e) {
      const errMsg = String((e && e.message) || e);
      log(`  [错误] ${f.name}: ${errMsg}`);
      errors.push({ doc: stateKey(f.relative_path, f.name), error: errMsg });
    }
  }

  // 飞书端已删除的文档:从增量状态里清掉
  const currentKeys = new Set(
    files.map((f) => stateKey(f.relative_path, f.name))
  );
  for (const key of Object.keys(state)) {
    if (!currentKeys.has(key)) delete state[key];
  }

  saveSyncState(kb_root, dest_prefix, state);

  // present:同步后应该存在的全部文档(本轮转换的 + 增量跳过但仍有效的)
  const present = new Set();
  for (const info of Object.values(state)) {
    if (info.rel_path) present.add(info.rel_path);
  }

  // 严格镜像:src/<dest_prefix>/ 下不在 present 里的文件一律删除,
  // 连同 docs/ 侧的 md 镜像与 .assets 伴生目录。
  let purged = 0;
  for (const rel of walkFiles(srcBase)) {
    const relPath = `${dest_prefix}/${rel}`;
    if (present.has(relPath)) continue;
    try {
      fs.rmSync(path.join(kb_root, 'src', relPath), { force: true });
      fs.rmSync(path.join(kb_root, 'docs', relPath), { force: true });
      const assetsRel = relPath.replace(/\.[^./]*$/, '') + '.assets';
      fs.rmSync(path.join(kb_root, 'docs', assetsRel), {
        recursive: true,
        force: true,
      });
      purged += 1;
    } catch (e) {
      log(`  [警告] 严格镜像清理失败 ${relPath}: ${e}`);
    }
  }
  // 清掉空目录(从深到浅)
  const dirs = [];
  (function collectDirs(base, rel = '') {
    const dir = rel ? path.join(base, rel) : base;
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch (e) {
      return;
    }
    for (const e of entries) {
      if (e.isDirectory()) {
        const r = rel ? `${rel}/${e.name}` : e.name;
        dirs.push(r);
        collectDirs(base, r);
      }
    }
  })(srcBase);
  dirs.sort((a, b) => b.length - a.length);
  for (const d of dirs) {
    try {
      const abs = path.join(srcBase, d);
      if (fs.readdirSync(abs).length === 0) fs.rmdirSync(abs);
    } catch (e) {
      // ignore
    }
  }

  log(
    `\n完成! 同步 ${documents.length} 个, 跳过 ${skipped} 个未变更, ` +
      `失败 ${errors.length} 个, 严格镜像清理 ${purged} 个`
  );

  return {
    ok: true,
    documents,
    present: Array.from(present),
    skipped,
    errors,
  };
}

// ==========================================================
//  入口
// ==========================================================

function readStdin() {
  return new Promise((resolve, reject) => {
    let buf = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (c) => (buf += c));
    process.stdin.on('end', () => resolve(buf));
    process.stdin.on('error', reject);
  });
}

(async () => {
  let out;
  try {
    const raw = await readStdin();
    const req = JSON.parse(raw);
    out = await run(req);
  } catch (err) {
    out = { ok: false, error: String((err && err.message) || err) };
  }
  process.stdout.write(JSON.stringify(out));
})();
