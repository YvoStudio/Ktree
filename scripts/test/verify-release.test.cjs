const test = require('node:test');
const assert = require('node:assert/strict');
const { expectedAssets, validateRelease, findRelease } = require('../verify-release.cjs');

function fixture() {
  const repo = 'YvoStudio/Ktree', tag = 'v0.1.27';
  const keyId = Buffer.from('0123456789abcdef', 'hex');
  const encoded = (size, label) => {
    const bytes = Buffer.alloc(size);
    bytes.write('Ed'); keyId.copy(bytes, 2);
    return Buffer.from(`untrusted comment: ${label}\n${bytes.toString('base64')}\n`).toString('base64');
  };
  const pubkey = encoded(42, 'test public key');
  const signature = encoded(74, 'test signature');
  const names = expectedAssets('0.1.27');
  const release = {
    tag_name: tag, prerelease: false, draft: true,
    assets: names.map((name, id) => ({ name, id, size: 100, state: 'uploaded' })),
  };
  const platforms = {
    'darwin-aarch64': 'Ktree_aarch64.app.tar.gz',
    'windows-x86_64': 'Ktree_0.1.27_x64-setup.exe',
    'linux-x86_64': 'Ktree_0.1.27_amd64.AppImage',
  };
  const updater = {
    version: '0.1.27', pub_date: '2026-10-08T00:00:00Z',
    platforms: Object.fromEntries(Object.entries(platforms).map(([platform, name]) => [platform, {
      url: `https://github.com/${repo}/releases/download/${tag}/${name}`, signature,
    }])),
  };
  const signatures = Object.fromEntries(names.filter(name => name.endsWith('.sig')).map(name => [name, signature]));
  return { repo, tag, release, updater, signatures, pubkey };
}

test('三平台完整草稿才可发布;已发布完整版本可重复校验', () => {
  const data = fixture();
  assert.equal(validateRelease(data).assets, 14);
  data.release.draft = false;
  assert.equal(validateRelease(data).version, '0.1.27');
});

test('认证列表可分页找到草稿,不存在的标签拒绝发布', () => {
  const { repo, tag, release } = fixture();
  const requests = [];
  const api = endpoint => {
    requests.push(endpoint);
    return endpoint.endsWith('page=1')
      ? Array.from({ length: 100 }, (_, index) => ({ tag_name: `other-${index}` }))
      : [release];
  };
  assert.equal(findRelease(repo, tag, api), release);
  assert.deepEqual(requests, [
    `repos/${repo}/releases?per_page=100&page=1`,
    `repos/${repo}/releases?per_page=100&page=2`,
  ]);
  assert.throws(() => findRelease(repo, tag, () => []), /未找到 Release/);
});

test('Mac 构建失败造成的残缺 Release 必须拒绝发布', () => {
  const data = fixture();
  data.release.assets = data.release.assets.filter(asset => !/aarch64/.test(asset.name));
  delete data.updater.platforms['darwin-aarch64'];
  assert.throws(() => validateRelease(data), /缺少发布附件/);
});

test('有 Mac 附件但清单没有 Mac 平台也不能发布', () => {
  const data = fixture();
  delete data.updater.platforms['darwin-aarch64'];
  assert.throws(() => validateRelease(data), /缺少更新平台: darwin-aarch64/);
});

test('不能用其他平台的安装包冒充 Mac 更新包', () => {
  const data = fixture();
  data.updater.platforms['darwin-aarch64'] = { ...data.updater.platforms['windows-x86_64'] };
  assert.throws(() => validateRelease(data), /更新产物与平台不匹配: darwin-aarch64/);
});

test('清单版本或下载地址指向旧版本时拒绝发布', () => {
  const data = fixture();
  data.updater.version = '0.1.26';
  assert.throws(() => validateRelease(data), /更新清单版本不匹配/);
  data.updater.version = '0.1.27';
  data.updater.platforms['darwin-aarch64'].url = data.updater.platforms['darwin-aarch64'].url.replace('v0.1.27', 'v0.1.26');
  assert.throws(() => validateRelease(data), /更新地址未指向本版本/);
});

test('缺少签名、签名不一致或公钥 ID 不一致都不能发布', () => {
  const data = fixture();
  const name = 'Ktree_aarch64.app.tar.gz.sig';
  delete data.signatures[name];
  assert.throws(() => validateRelease(data), /清单与附件签名不一致/);
  data.signatures[name] = data.updater.platforms['darwin-aarch64'].signature;
  const text = Buffer.from(data.pubkey, 'base64').toString();
  const bytes = Buffer.from(text.trim().split('\n')[1], 'base64');
  bytes[2] ^= 1;
  data.pubkey = Buffer.from(`untrusted comment: test key\n${bytes.toString('base64')}\n`).toString('base64');
  assert.throws(() => validateRelease(data), /签名与应用公钥不匹配/);
});

test('空附件或尚未上传完成的附件不能发布', () => {
  const data = fixture();
  data.release.assets[0].size = 0;
  assert.throws(() => validateRelease(data), /发布附件未上传完整/);
  data.release.assets[0].size = 100;
  data.release.assets[0].state = 'new';
  assert.throws(() => validateRelease(data), /发布附件未上传完整/);
});

test('预发布、非版本标签、无效日期或失效平台别名不能进入正式更新源', () => {
  const data = fixture();
  data.release.prerelease = true;
  assert.throws(() => validateRelease(data), /预发布版本/);
  data.release.prerelease = false;
  assert.throws(() => validateRelease({ ...data, tag: 'main' }), /vX.Y.Z/);
  data.updater.pub_date = 'invalid';
  assert.throws(() => validateRelease(data), /日期无效/);
  data.updater.pub_date = '2026-10-08T00:00:00Z';
  data.updater.platforms['darwin-aarch64-app'] = {
    ...data.updater.platforms['darwin-aarch64'],
    url: 'https://github.com/YvoStudio/Ktree/releases/download/v0.1.27/missing.app.tar.gz',
  };
  assert.throws(() => validateRelease(data), /更新产物不存在/);
});
