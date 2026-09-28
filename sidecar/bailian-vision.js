// 百炼 OpenAI 兼容视觉接口。仅在用户配置密钥后调用；PPTX 文件本身不上传。

const MAX_IMAGE_BYTES = 15 * 1024 * 1024;
const MIME = { png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp', bmp: 'image/bmp' };

function validateVision(config) {
  if (!config || !config.api_key) return null;
  const baseUrl = String(config.base_url || '').trim().replace(/\/+$/, '');
  const model = String(config.model || '').trim();
  const apiKey = String(config.api_key || '').trim();
  const url = new URL(baseUrl);
  if (url.protocol !== 'https:' || url.username || url.password || !url.hostname) {
    throw new Error('百炼兼容地址必须是有效的 HTTPS URL');
  }
  if (!/^[A-Za-z0-9._:-]{1,100}$/.test(model)) throw new Error('百炼模型名称格式不正确');
  return { url: `${baseUrl}/chat/completions`, model, apiKey };
}

function parseContent(content) {
  const text = String(content || '').trim().replace(/^```(?:json)?\s*/i, '').replace(/\s*```$/, '');
  const value = JSON.parse(text);
  return {
    description: typeof value.description === 'string' ? value.description.trim().slice(0, 1000) : '',
    visibleText: typeof value.visible_text === 'string' ? value.visible_text.trim().slice(0, 3000) : '',
  };
}

function createImageAnalyzer(config) {
  const settings = validateVision(config);
  if (!settings) return null;
  return async (bytes, format) => {
    const mime = MIME[format];
    if (!mime) throw new Error(`百炼暂不支持 ${format} 图片`);
    if (bytes.length > MAX_IMAGE_BYTES) throw new Error('图片超过百炼单图安全上限，已跳过语义分析');

    const response = await fetch(settings.url, {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${settings.apiKey}`,
        'Content-Type': 'application/json; charset=utf-8',
      },
      body: JSON.stringify({
        model: settings.model,
        messages: [{ role: 'user', content: [
          { type: 'text', text: '请只根据图片本身，用中文返回 JSON：{"description":"图片内容、图表趋势和关系的简要客观描述","visible_text":"图片里可辨认的全部重要文字"}。不要推断图片之外的事实；看不清的文字不要编造。' },
          { type: 'image_url', image_url: { url: `data:${mime};base64,${Buffer.from(bytes).toString('base64')}` } },
        ] }],
        response_format: { type: 'json_object' },
        enable_thinking: false,
        temperature: 0.1,
        max_completion_tokens: 600,
      }),
      signal: AbortSignal.timeout(45_000),
    });
    const raw = await response.text();
    let data;
    try { data = raw ? JSON.parse(raw) : {}; }
    catch { throw new Error(`百炼返回了无法解析的响应（HTTP ${response.status}）`); }
    if (!response.ok) {
      const message = typeof data?.error?.message === 'string'
        ? data.error.message.replaceAll(settings.apiKey, '[密钥已隐藏]').slice(0, 240)
        : `HTTP ${response.status}`;
      throw new Error(`百炼请求失败：${message}`);
    }
    const content = data?.choices?.[0]?.message?.content;
    if (typeof content !== 'string' || !content.trim()) throw new Error('百炼没有返回图片描述');
    return parseContent(content);
  };
}

module.exports = { createImageAnalyzer, parseContent, validateVision };
