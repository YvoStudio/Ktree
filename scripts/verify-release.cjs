const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const REQUIRED_PLATFORMS = ['darwin-aarch64', 'windows-x86_64', 'linux-x86_64'];

function expectedAssets(version) {
  return [
    `Ktree_${version}_aarch64.dmg`, 'Ktree_aarch64.app.tar.gz', 'Ktree_aarch64.app.tar.gz.sig',
    `Ktree_${version}_x64-setup.exe`, `Ktree_${version}_x64-setup.exe.sig`,
    `Ktree_${version}_x64_en-US.msi`, `Ktree_${version}_x64_en-US.msi.sig`,
    `Ktree_${version}_amd64.AppImage`, `Ktree_${version}_amd64.AppImage.sig`,
    `Ktree_${version}_amd64.deb`, `Ktree_${version}_amd64.deb.sig`,
    `Ktree-${version}-1.x86_64.rpm`, `Ktree-${version}-1.x86_64.rpm.sig`, 'latest.json',
  ];
}

function signingKeyId(encoded, length, label) {
  assert.equal(typeof encoded, 'string', `${label} 必须是字符串`);
  const text = Buffer.from(encoded, 'base64').toString('utf8');
  const bytes = Buffer.from(text.trim().split(/\r?\n/)[1] || '', 'base64');
  assert.equal(bytes.length, length, `${label} 格式不正确`);
  assert.ok(['Ed', 'ED'].includes(bytes.subarray(0, 2).toString()), `${label} 算法不正确`);
  return bytes.subarray(2, 10).toString('hex');
}

// 草稿和已发布版本都可核对,但缺少任一平台/附件/签名就拒绝提升为 Latest。
function validateRelease({ release, updater, signatures, repo, tag, pubkey }) {
  assert.match(repo, /^[\w.-]+\/[\w.-]+$/, '仓库格式必须是 owner/repo');
  assert.match(tag, /^v\d+\.\d+\.\d+$/, '正式发布必须使用 vX.Y.Z 标签');
  const version = tag.slice(1);
  assert.equal(release.tag_name, tag, 'Release 标签不匹配');
  assert.equal(release.prerelease, false, '正式更新源不能是预发布版本');
  assert.equal(updater.version?.replace(/^v/, ''), version, '更新清单版本不匹配');
  assert.ok(!Number.isNaN(Date.parse(updater.pub_date)), '更新清单日期无效');
  const assets = new Map(release.assets.map(asset => [asset.name, asset]));
  for (const name of expectedAssets(version)) {
    const asset = assets.get(name);
    assert.ok(asset, `缺少发布附件: ${name}`);
    assert.ok(asset.size > 0 && asset.state === 'uploaded', `发布附件未上传完整: ${name}`);
  }
  for (const platform of REQUIRED_PLATFORMS) {
    assert.ok(updater.platforms?.[platform], `缺少更新平台: ${platform}`);
  }
  const updaterAssets = {
    'darwin-aarch64': ['Ktree_aarch64.app.tar.gz'],
    'windows-x86_64': [`Ktree_${version}_x64-setup.exe`, `Ktree_${version}_x64_en-US.msi`],
    'linux-x86_64': [`Ktree_${version}_amd64.AppImage`, `Ktree_${version}_amd64.deb`, `Ktree-${version}-1.x86_64.rpm`],
  };
  const keyId = signingKeyId(pubkey, 42, '应用更新公钥');
  for (const [platform, target] of Object.entries(updater.platforms)) {
    const url = new URL(target.url);
    const prefix = `/${repo}/releases/download/${tag}/`;
    assert.equal(url.origin, 'https://github.com', `更新域名不正确: ${platform}`);
    assert.ok(url.pathname.startsWith(prefix), `更新地址未指向本版本: ${platform}`);
    const name = decodeURIComponent(url.pathname.slice(prefix.length));
    assert.ok(!name.includes('/') && assets.has(name), `更新产物不存在: ${platform}`);
    const base = REQUIRED_PLATFORMS.find(item => platform === item || platform.startsWith(item + '-'));
    assert.ok(base && updaterAssets[base].includes(name), `更新产物与平台不匹配: ${platform}`);
    assert.ok(assets.has(name + '.sig'), `更新签名附件不存在: ${platform}`);
    assert.equal(typeof target.signature, 'string', `缺少更新签名: ${platform}`);
    assert.equal(target.signature.trim(), signatures[name + '.sig']?.trim(), `清单与附件签名不一致: ${platform}`);
    assert.equal(signingKeyId(target.signature, 74, `更新签名 ${platform}`), keyId, `签名与应用公钥不匹配: ${platform}`);
  }
  return { version, platforms: REQUIRED_PLATFORMS, assets: assets.size };
}

function githubApi(endpoint, raw = false) {
  const args = ['api', endpoint];
  if (raw) args.push('--header', 'Accept: application/octet-stream');
  const output = execFileSync('gh', args, {
    encoding: 'utf8', timeout: 30000, maxBuffer: 4 * 1024 * 1024,
  });
  return raw ? output : JSON.parse(output);
}

// 认证列表同时定位草稿与正式版本,并处理分页。
function findRelease(repo, tag, api = githubApi) {
  for (let page = 1; ; page++) {
    const releases = api(`repos/${repo}/releases?per_page=100&page=${page}`);
    const release = releases.find(item => item.tag_name === tag);
    if (release) return release;
    assert.equal(releases.length, 100, `未找到 Release: ${tag}`);
  }
}

// 通过认证的资产 API 读取草稿附件;草稿的 browser_download_url 尚未公开。
function loadRelease(repo, tag) {
  const release = findRelease(repo, tag);
  const assets = new Map(release.assets.map(asset => [asset.name, asset]));
  assert.ok(assets.has('latest.json'), '缺少更新清单 latest.json');
  const readAsset = name => {
    const asset = assets.get(name);
    assert.ok(asset, `缺少发布附件: ${name}`);
    return githubApi(`repos/${repo}/releases/assets/${asset.id}`, true);
  };
  const updater = JSON.parse(readAsset('latest.json'));
  const signatures = {};
  for (const target of Object.values(updater.platforms || {})) {
    const name = decodeURIComponent(new URL(target.url).pathname.split('/').pop()) + '.sig';
    if (!(name in signatures)) signatures[name] = readAsset(name);
  }
  return { release, updater, signatures };
}

function main(args = process.argv.slice(2)) {
  const option = name => args[args.indexOf(name) + 1];
  const repo = args.includes('--repo') ? option('--repo') : process.env.GITHUB_REPOSITORY;
  const tag = args.includes('--tag') ? option('--tag') : process.env.RELEASE_TAG;
  assert.ok(repo && tag, '用法: node scripts/verify-release.cjs --repo owner/repo --tag vX.Y.Z');
  assert.match(repo, /^[\w.-]+\/[\w.-]+$/);
  assert.match(tag, /^v\d+\.\d+\.\d+$/);
  const root = path.resolve(__dirname, '..');
  const config = JSON.parse(fs.readFileSync(path.join(root, 'src-tauri/tauri.conf.json'), 'utf8'));
  const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
  assert.equal(config.version, tag.slice(1), 'Tauri 版本与发布标签不匹配');
  assert.equal(pkg.version, config.version, 'npm 与 Tauri 版本不匹配');
  const data = loadRelease(repo, tag);
  const result = validateRelease({ ...data, repo, tag, pubkey: config.plugins.updater.pubkey });
  console.log(`发布完整性校验通过: v${result.version}, ${result.platforms.join(', ')}, ${result.assets} 个附件`);
}

module.exports = { REQUIRED_PLATFORMS, expectedAssets, validateRelease, findRelease, loadRelease };
if (require.main === module) {
  try { main(); } catch (error) { console.error(`拒绝发布 Latest: ${error.message}`); process.exitCode = 1; }
}
