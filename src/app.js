/* ============================================================
 * ChatGPT 安装助手 — 前端逻辑（零依赖）
 * 步骤切换 + Tauri IPC。直接用浏览器打开时进入演示模式（假数据），仅用于预览界面。
 * ============================================================ */
'use strict';

const TAURI = (window.__TAURI__ && window.__TAURI__.core) ? window.__TAURI__ : null;
const DEMO = !TAURI;
if (DEMO) document.body.classList.add('demo');

const $ = id => document.getElementById(id);
const esc = s => String(s == null ? '' : s).replace(/[&<>"']/g, c =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const icon = (name, cls = 'ic') => `<svg class="${cls}"><use href="#i-${name}"/></svg>`;
const sleep = ms => new Promise(r => setTimeout(r, ms));
const fmtMB = b => (b / 1048576).toFixed(1);
function normErr(e, fallbackCode = 'UNKNOWN') {
  if (e && e.code) return e;
  return { code: fallbackCode, message: String((e && e.message) || e || '未知错误'), hint: '' };
}

/* ---------------- IPC ---------------- */
const demoListeners = {};
function onEvent(name, cb) {
  if (DEMO) { demoListeners[name] = cb; return; }
  TAURI.event.listen(name, e => cb(e.payload));
}
function fireDemo(name, payload) { const cb = demoListeners[name]; if (cb) cb(payload); }
async function invoke(cmd, args) {
  if (DEMO) return demoInvoke(cmd, args || {});
  return TAURI.core.invoke(cmd, args || {});
}

/* ---------------- 演示模式假数据 ---------------- */
const demo = { dl: 0, total: 786.4 * 1048576, timer: null, dir: 'C:\\Users\\demo\\AppData\\Local\\com.github.chatgpt-installer' };
async function demoInvoke(cmd, args) {
  await sleep(cmd === 'check_env' ? 900 : cmd === 'verify_package' ? 1100 : 300);
  switch (cmd) {
    case 'app_info': return { version: '1.2.0', arch: 'x86_64', admin: false };
    case 'take_resume_action': return null;
    case 'get_pkg_dir': return demo.dir;
    case 'pick_pkg_dir': throw { code: 'NO_DIALOG', message: '演示模式无系统对话框' };
    case 'set_pkg_dir': {
      const p = (args.path || '').trim();
      if (!p) throw { code: 'BAD_DIR', message: '目录不能为空', hint: '' };
      if (!/^[a-zA-Z]:[\\/]/.test(p)) throw { code: 'BAD_DIR', message: '请填写绝对路径', hint: '例如 D:\\Downloads' };
      demo.dir = p; demo.dl = 0; return p;
    }
    case 'check_env': return [
      { id: 'os', title: '操作系统', detail: 'Windows 11 24H2（Build 26100），满足要求', status: 'ok' },
      { id: 'arch', title: 'CPU 架构', detail: 'x64，已自动匹配安装包 ChatGPT-x64.msix', status: 'ok' },
      { id: 'disk', title: '磁盘空间', detail: 'C: 盘 可用 128.4 GB（需要 ≥ 3 GB）', status: 'ok' },
      { id: 'net', title: '网络连通', detail: '官方 CDN 可达，延迟约 86ms', status: 'ok' },
      { id: 'admin', title: '管理员权限', detail: '当前为普通权限：通常可直接为当前用户安装，必要时会请求一次提权', status: 'warn' },
    ];
    case 'start_download': {
      const tick = () => {
        demo.dl = Math.min(demo.total, demo.dl + demo.total * (0.02 + Math.random() * 0.02));
        fireDemo('download-progress', { downloaded: demo.dl, total: demo.total, percent: demo.dl / demo.total * 100 });
        if (demo.dl < demo.total) demo.timer = setTimeout(tick, 250);
        else fireDemo('download-done', { total: demo.total });
      };
      const from = demo.dl;
      demo.timer = setTimeout(tick, 100);
      return { url: 'https://persistent.oaistatic.com/codex-app-prod/ChatGPT-x64.msix', dest: demo.dir + '\\ChatGPT-x64.msix', resumed_from: from, total: 0, complete: false };
    }
    case 'pause_download':
      clearTimeout(demo.timer);
      setTimeout(() => fireDemo('download-paused', {}), 400);
      return null;
    case 'discard_package': demo.dl = 0; return null;
    case 'verify_package': return {
      signature: 'Valid', publisher: '50BDFD77-8903-4850-9FFE-6E8522F64D5B', issuer: 'Microsoft Marketplace CA G 024',
      identity: 'OpenAI.Codex', version: '26.915.4065.0', size: 824570408 };
    case 'install_package': {
      for (const [s, t, skipped] of [[1, '正在为当前用户注册应用包…'], [2, '普通权限下跳过整机预置（不影响使用）', true], [3, '安装完成']]) {
        await sleep(1000); fireDemo('install-progress', { stage: s, text: t, skipped: !!skipped });
      }
      return null;
    }
    case 'check_update': return {
      installed: true, installed_version: '1.2025.301.0', latest_version: '26.1007.2314.0', has_update: true,
      note: '发现新版本 v26.1007.2314.0（本机 v1.2025.301.0）' };
    case 'uninstall_package_cmd': throw { code: 'NEED_ELEVATION', message: '需要管理员权限', hint: '' };
    case 'restart_elevated_cmd': throw { code: 'UAC_CANCELLED', message: '未获得管理员权限', hint: '演示模式：模拟用户在 UAC 中点了「否」。' };
    case 'get_last_log': return '[demo] log line 1\n[demo] log line 2';
    case 'launch_app': case 'exit_app': return null;
  }
  return null;
}

/* ---------------- 状态 ---------------- */
const STEP_NAMES = ['欢迎', '环境检测', '下载', '安全校验', '安装', '完成'];
const S = {
  cur: -1,
  failedStep: -1,
  retry: null,
  envChecking: false,
  envOk: false,
  dl: { running: false, pausing: false, paused: false, done: false, samples: [] },
  verifying: false,
  verified: null,
  installing: false,
  warnings: [],
  update: null,       // check_update 结果
  updateChecking: false,
  autoClose: null,
};

/* ---------------- 通用 UI：对话框 / 提示 / 忙碌 ---------------- */
function toast(text, ms = 2200) {
  const t = document.createElement('div');
  t.className = 'toast'; t.textContent = text;
  document.body.appendChild(t);
  setTimeout(() => t.remove(), ms);
}
function busy(text) {
  if (text === false) { $('busy').hidden = true; return; }
  $('busyText').textContent = text || '正在处理…';
  $('busy').hidden = false;
}
/**
 * 应用内对话框，替代 alert / confirm（WebView2 原生弹窗标题是 “tauri.localhost 显示”）。
 * opts: { title, text, tone: ''|'warn'|'danger', icon, ok, cancel(false=无取消), okClass,
 *         checkbox: '文案', input: { value, placeholder }, validate: async (value) => 错误文本|null }
 * 返回 Promise<{ ok, checked, value }>
 */
function dialog(opts) {
  return new Promise(resolve => {
    const mask = document.createElement('div');
    mask.className = 'mask';
    const ic = opts.icon || (opts.tone === 'danger' ? 'trash' : opts.tone === 'warn' ? 'alert' : 'info');
    mask.innerHTML = `
      <div class="dialog" role="dialog" aria-modal="true">
        <div class="db">
          <h3 class="${opts.tone || ''}">${icon(ic)}${esc(opts.title)}</h3>
          <div class="dt">${esc(opts.text || '').replace(/\n/g, '<br>')}
            ${opts.input ? `<input type="text" spellcheck="false" placeholder="${esc(opts.input.placeholder || '')}" value="${esc(opts.input.value || '')}"><div class="derr"></div>` : ''}
            ${opts.checkbox ? `<label class="check"><input type="checkbox"><span>${esc(opts.checkbox)}</span></label>` : ''}
          </div>
        </div>
        <div class="df">
          ${opts.cancel === false ? '' : `<button class="btn sec" data-r="0">${esc(opts.cancel || '取消')}</button>`}
          <button class="btn ${opts.okClass || 'pri'}" data-r="1">${esc(opts.ok || '确定')}</button>
        </div>
      </div>`;
    document.body.appendChild(mask);
    const input = mask.querySelector('input[type=text]');
    const cb = mask.querySelector('input[type=checkbox]');
    const okBtn = mask.querySelector('[data-r="1"]');
    const close = ok => {
      document.removeEventListener('keydown', onKey, true);
      mask.remove();
      resolve({ ok, checked: !!(cb && cb.checked), value: input ? input.value.trim() : '' });
    };
    const submit = async () => {
      if (opts.validate) {
        okBtn.disabled = true;
        const err = await opts.validate(input ? input.value.trim() : '');
        okBtn.disabled = false;
        if (err) { mask.querySelector('.derr').textContent = err; return; }
      }
      close(true);
    };
    const onKey = e => {
      if (e.key === 'Escape' && opts.cancel !== false) { e.preventDefault(); close(false); }
      if (e.key === 'Enter' && document.activeElement && document.activeElement.tagName !== 'BUTTON') { e.preventDefault(); submit(); }
    };
    document.addEventListener('keydown', onKey, true);
    mask.addEventListener('click', e => {
      const r = e.target.closest('[data-r]');
      if (!r) return;
      if (r.dataset.r === '1') submit(); else close(false);
    });
    // 危险操作默认聚焦「取消」，防止回车误触
    const focusEl = input || (opts.tone === 'danger' && mask.querySelector('[data-r="0"]')) || okBtn;
    focusEl.focus({ focusVisible: false });
    if (input) input.select();
  });
}

/* ---------------- 布局：步骤条 / 底栏 ---------------- */
const prevBtn = $('prevBtn'), auxBtn = $('auxBtn'), nextBtn = $('nextBtn');

function renderStepper() {
  const edge = S.cur === 6 ? S.failedStep : S.cur === 5 ? STEP_NAMES.length : S.cur;
  $('stepper').innerHTML = STEP_NAMES.map((n, i) => {
    let cls = '';
    if (i < edge) cls = 'done';
    else if (i === edge) cls = S.cur === 6 ? 'err' : 'now';
    return `<div class="st ${cls}"><i></i><span>${i + 1}. ${n}</span></div>`;
  }).join('');
}

function setBtn(btn, spec) {
  if (!spec) { btn.hidden = true; btn.onclick = null; return; }
  btn.hidden = false;
  btn.innerHTML = (spec.icon && !spec.iconRight ? icon(spec.icon) : '') + esc(spec.text) + (spec.icon && spec.iconRight ? icon(spec.icon) : '');
  btn.disabled = !!spec.disabled;
  btn.title = spec.title || '';
  btn.onclick = spec.onClick || null;
}
function setStatus(html, cls = '') {
  const el = $('status');
  el.className = 'status ' + cls;
  el.innerHTML = html || '';
}

/** 根据当前步骤与状态刷新底栏（每个页面只有一个主按钮）。 */
function renderFooter() {
  const back = { text: '上一步', onClick: () => go(S.cur - 1) };
  switch (S.cur) {
    case 0: {
      const u = S.update;
      const text = u && u.has_update ? '开始更新' : (u && u.installed ? '重新安装' : '开始安装');
      setBtn(prevBtn, null);
      setBtn(auxBtn, null);
      setBtn(nextBtn, { text, icon: 'arrow', iconRight: true, onClick: () => go(1) });
      renderUpdateStatus();
      break;
    }
    case 1:
      setStatus('');
      setBtn(prevBtn, back);
      setBtn(auxBtn, { text: '重新检测', icon: 'refresh', disabled: S.envChecking, onClick: runEnvCheck });
      setBtn(nextBtn, { text: '下一步', icon: 'arrow', iconRight: true, disabled: !S.envOk || S.envChecking, onClick: () => go(2) });
      break;
    case 2: {
      const d = S.dl;
      setBtn(prevBtn, back);
      if (d.done) setBtn(auxBtn, null);
      else if (d.pausing) setBtn(auxBtn, { text: '正在暂停…', icon: 'pause', disabled: true });
      else if (d.paused) setBtn(auxBtn, { text: '继续', icon: 'play', onClick: resumeDownload });
      else setBtn(auxBtn, { text: '暂停', icon: 'pause', disabled: !d.running, onClick: pauseDownload });
      setBtn(nextBtn, { text: '下一步', icon: 'arrow', iconRight: true, disabled: !d.done, title: d.done ? '' : '下载完成后可继续', onClick: () => go(3) });
      if (!d.running && !d.paused && !d.done) setStatus('');
      break;
    }
    case 3:
      setStatus('');
      setBtn(prevBtn, back);
      setBtn(auxBtn, null);
      setBtn(nextBtn, { text: '安装', icon: 'arrow', iconRight: true, disabled: !S.verified, onClick: doInstall });
      break;
    case 4:
      setStatus('<span>安装进行中，请勿关闭窗口</span>');
      setBtn(prevBtn, null); setBtn(auxBtn, null); setBtn(nextBtn, null);
      break;
    case 5:
      setBtn(prevBtn, null);
      setBtn(auxBtn, { text: '关闭', onClick: () => invoke('exit_app') });
      setBtn(nextBtn, { text: '启动 ChatGPT', icon: 'arrow', iconRight: true, onClick: launchApp });
      break;
    case 6:
      setStatus('');
      setBtn(prevBtn, { text: '返回首页', onClick: () => go(0) });
      setBtn(auxBtn, { text: '复制诊断日志', icon: 'copy', onClick: copyLog });
      setBtn(nextBtn, { text: '重试', icon: 'refresh', onClick: () => { const r = S.retry; if (r) r(); } });
      break;
  }
}

function go(n) {
  clearAutoClose();
  S.cur = n;
  document.querySelectorAll('.step').forEach(s => s.classList.toggle('active', +s.dataset.s === n));
  $('main').scrollTop = 0;
  renderStepper();
  renderFooter();
  if (n === 0 && !S.update && !S.updateChecking) checkUpdate(true);
  if (n === 1 && !S.envOk && !S.envChecking) runEnvCheck();
  if (n === 2) enterDownload();
  if (n === 3) enterVerify();
  if (n === 5) armAutoClose();
}

/* ---------------- 欢迎页：检查更新 ---------------- */
function renderUpdateStatus() {
  if (S.cur !== 0) return;
  const u = S.update;
  if (S.updateChecking) setStatus('<div class="spin"></div><span>正在检查本机安装状态…</span>');
  else if (!u) setStatus('');
  else if (u.error) setStatus(`${icon('alert')}<span title="${esc(u.error)}">检查更新失败，可稍后重试</span>`);
  else if (u.has_update) setStatus(`${icon('up')}<span title="${esc(u.note)}">${esc(u.note)}</span>`, 'hl');
  else if (u.installed) setStatus(`${icon('check')}<span title="${esc(u.note)}">${esc(u.note)}</span>`, 'ok');
  else setStatus(`${icon('info')}<span>${esc(u.note)}</span>`);
}
async function checkUpdate(silent) {
  if (S.updateChecking) return;
  S.updateChecking = true;
  renderUpdateStatus();
  try {
    S.update = await invoke('check_update');
    if (!silent && S.update && !S.update.has_update) toast(S.update.installed ? '已是最新版本' : '本机尚未安装 ChatGPT');
  } catch (e) {
    S.update = { error: normErr(e).message };
  }
  S.updateChecking = false;
  if (S.cur === 0) renderFooter();
}
$('checkUpdateBtn').addEventListener('click', () => checkUpdate(false));

/* ---------------- 1 环境检测 ---------------- */
const BADGE = { ok: ['ok', 'check'], warn: ['warn', 'info'], fail: ['fail', 'x'], skip: ['skip', 'minus'] };
function rowHtml(status, title, detailHtml) {
  const [cls, ic] = BADGE[status] || BADGE.warn;
  return `<div class="row"><div class="badge ${cls}">${icon(ic)}</div><div><div class="tt">${esc(title)}</div><div class="ds selectable">${detailHtml}</div></div></div>`;
}
const skeleton = n => Array.from({ length: n }, () =>
  '<div class="row skeleton"><div class="badge"></div><div><div class="tt">.</div><div class="ds">.</div></div></div>').join('');

async function runEnvCheck() {
  S.envChecking = true; S.envOk = false;
  $('envList').innerHTML = skeleton(5);
  if (S.cur === 1) renderFooter();
  try {
    const items = await invoke('check_env');
    $('envList').innerHTML = items.map(it => rowHtml(it.status, it.title, esc(it.detail))).join('');
    S.envOk = !items.some(it => it.status === 'fail');
  } catch (e) {
    $('envList').innerHTML = rowHtml('fail', '检测失败', esc(normErr(e).message));
  }
  S.envChecking = false;
  if (S.cur === 1) renderFooter();
}

/* ---------------- 2 下载 ---------------- */
async function loadPkgDir() {
  try {
    const d = await invoke('get_pkg_dir');
    $('dlDir').textContent = d; $('dlDir').title = d;
  } catch (e) { $('dlDir').textContent = '读取失败'; }
}
function enterDownload() {
  loadPkgDir();
  const d = S.dl;
  if (!d.running && !d.paused && !d.done) startDownload();
  renderDlUi();
}
function renderDlUi() {
  const d = S.dl;
  const wrap = $('dlBarWrap');
  wrap.classList.toggle('live', d.running && !d.pausing);
  wrap.classList.toggle('paused', d.paused);
  $('changeDirBtn').disabled = d.running;
  $('changeDirBtn').title = d.running ? '下载中不可更改，请先暂停' : '';
  if (S.cur === 2) renderFooter();
}
function resetDl() {
  Object.assign(S.dl, { running: false, pausing: false, paused: false, done: false, samples: [] });
  S.verified = null;
  $('dlBar').style.width = '0';
  $('dlNums').innerHTML = '<span>准备下载…</span>';
}
async function startDownload() {
  const d = S.dl;
  if (d.running) return;
  Object.assign(d, { running: true, pausing: false, paused: false, done: false, samples: [] });
  S.verified = null;
  renderDlUi();
  $('dlNums').innerHTML = '<span>正在连接官方 CDN…</span>';
  try {
    const info = await invoke('start_download');
    $('dlName').textContent = info.dest.split(/[/\\]/).pop();
    if (info.complete) { onDlDone({ total: info.total }, true); return; }
    if (info.resumed_from > 0) $('dlNums').innerHTML = `<span>从 ${fmtMB(info.resumed_from)} MB 处继续下载…</span>`;
  } catch (e) {
    d.running = false;
    renderDlUi();
    showError(e, 2, () => { resetDl(); go(2); });
  }
}
function fmtEta(sec) {
  if (!isFinite(sec) || sec <= 0) return '';
  if (sec < 60) return `剩余约 ${Math.ceil(sec)} 秒`;
  if (sec < 3600) return `剩余约 ${Math.ceil(sec / 60)} 分钟`;
  return `剩余约 ${(sec / 3600).toFixed(1)} 小时`;
}
function onDlProgress(p) {
  const d = S.dl;
  if (!d.running) return;
  const now = performance.now();
  d.samples.push([now, p.downloaded]);
  while (d.samples.length > 2 && now - d.samples[0][0] > 5000) d.samples.shift();
  let speed = 0;
  if (d.samples.length > 1) {
    const [t0, b0] = d.samples[0];
    speed = (p.downloaded - b0) / ((now - t0) / 1000);
  }
  $('dlBar').style.width = (p.total ? p.percent : 0).toFixed(1) + '%';
  const right = speed > 0
    ? `<span class="r"><b>${(speed / 1048576).toFixed(1)} MB/s</b>${p.total ? ' · ' + fmtEta((p.total - p.downloaded) / speed) : ''}</span>`
    : '<span class="r">测速中…</span>';
  $('dlNums').innerHTML = `<span><b>${p.total ? p.percent.toFixed(1) + '%' : '—'}</b></span>` +
    `<span><b>${fmtMB(p.downloaded)}</b> / ${p.total ? fmtMB(p.total) + ' MB' : '未知大小'}</span>` + right;
}
function onDlDone(p, already) {
  Object.assign(S.dl, { running: false, pausing: false, paused: false, done: true });
  $('dlBar').style.width = '100%';
  $('dlNums').innerHTML = `<span><b>${already ? '安装包已就绪' : '下载完成'}</b></span>` +
    (p && p.total ? `<span>${fmtMB(p.total)} MB</span>` : '') + '<span class="r">即将进入安全校验…</span>';
  renderDlUi();
  if (S.cur === 2) setTimeout(() => { if (S.cur === 2 && S.dl.done) go(3); }, 900);
}
async function pauseDownload() {
  if (!S.dl.running) return;
  S.dl.pausing = true;
  renderDlUi();
  try { await invoke('pause_download'); } catch (e) { S.dl.pausing = false; renderDlUi(); }
}
function resumeDownload() {
  if (S.dl.pausing || S.dl.running) return;
  S.dl.paused = false;
  startDownload();
}
onEvent('download-progress', onDlProgress);
onEvent('download-done', p => onDlDone(p, false));
onEvent('download-paused', () => {
  Object.assign(S.dl, { running: false, pausing: false, paused: true, samples: [] });
  $('dlNums').innerHTML = '<span><b>已暂停</b></span><span>进度已保留，点击「继续」恢复下载</span>';
  renderDlUi();
});
onEvent('download-error', e => {
  S.dl.running = false; S.dl.pausing = false;
  renderDlUi();
  showError(e, 2, () => { resetDl(); go(2); });
});

/* 更改下载目录：优先系统文件夹选择器，失败时回退手动输入 */
async function manualDirDialog() {
  const cur = $('dlDir').textContent;
  const r = await dialog({
    title: '更改下载目录', icon: 'folder', ok: '保存',
    text: '请输入保存安装包的目录（绝对路径）：',
    input: { value: (cur === '读取失败' || cur === '读取中…') ? '' : cur, placeholder: '例如 D:\\Downloads' },
    validate: async v => {
      try { await invoke('set_pkg_dir', { path: v }); return null; }
      catch (e) { const er = normErr(e); return er.message + (er.hint ? '：' + er.hint : ''); }
    },
  });
  return r.ok;
}
$('changeDirBtn').addEventListener('click', async () => {
  if (S.dl.running) return;
  let changed = false;
  try {
    changed = !!(await invoke('pick_pkg_dir'));
  } catch (e) {
    const er = normErr(e);
    if (er.code === 'BUSY') { toast(er.message); return; }
    changed = await manualDirDialog();
  }
  if (!changed) return;
  await loadPkgDir();
  toast('下载目录已更改');
  // 新目录从头（或从该目录已有分片）开始下载
  resetDl();
  if (S.cur === 2) startDownload();
});

/* ---------------- 3 安全校验 ---------------- */
function renderVerified(v) {
  $('verifyList').innerHTML = [
    rowHtml('ok', '数字签名', esc(v.signature) + '（Authenticode 有效）'),
    rowHtml('ok', '发布者', `<span class="mono">CN=${esc(v.publisher)}</span>（OpenAI 商店发布者）`),
    rowHtml('ok', '证书颁发者', esc(v.issuer || 'Microsoft')),
    rowHtml('ok', '包标识与版本', `<span class="mono">${esc(v.identity)}</span> · v${esc(v.version)} · ${fmtMB(v.size)} MB`),
  ].join('');
}
async function enterVerify() {
  if (S.verified) { renderVerified(S.verified); renderFooter(); return; }
  if (S.verifying) return;
  S.verifying = true;
  $('verifyList').innerHTML = skeleton(4);
  try {
    const v = await invoke('verify_package');
    S.verified = v;
    renderVerified(v);
  } catch (e) {
    // 校验失败说明本地包有问题：重试时删除安装包重新下载
    showError(e, 3, async () => {
      try { await invoke('discard_package'); } catch (_) { /* 删除失败也继续，下载会覆盖 */ }
      resetDl(); go(2);
    });
  }
  S.verifying = false;
  if (S.cur === 3) renderFooter();
}

/* ---------------- 4 安装 ---------------- */
function resetStages() {
  document.querySelectorAll('#instStages .stage').forEach(el => {
    el.className = 'stage';
    el.querySelector('.dot').innerHTML = '<div class="pend"></div>';
    el.querySelector('.sub').textContent = '';
  });
  $('instBar').style.width = '0';
  $('instTxt').textContent = '准备安装…';
}
function setStage(k, state, sub) {
  const el = document.querySelector(`#instStages .stage[data-k="${k}"]`);
  if (!el || el.classList.contains('done') || el.classList.contains('skip')) return;
  el.className = 'stage ' + state;
  const dot = el.querySelector('.dot');
  if (state === 'run') dot.innerHTML = '<div class="spin"></div>';
  if (state === 'done') dot.innerHTML = `<div class="badge ok" style="width:22px;height:22px">${icon('check')}</div>`;
  if (state === 'skip') dot.innerHTML = `<div class="badge skip" style="width:22px;height:22px">${icon('minus')}</div>`;
  if (sub) el.querySelector('.sub').textContent = sub;
}
onEvent('install-progress', p => {
  if (S.cur !== 4) return;
  $('instTxt').textContent = p.text;
  if (p.stage <= 0) return;
  $('instBar').style.width = (p.stage / 3 * 100) + '%';
  for (let k = 1; k < p.stage; k++) setStage(k, 'done');
  if (p.skipped) setStage(p.stage, 'skip', '已跳过');
  else setStage(p.stage, p.stage >= 3 ? 'done' : 'run');
  if (p.stage >= 3) setTimeout(() => { if (S.cur === 4) go(5); }, 700);
});
onEvent('install-warning', w => { S.warnings.push(String(w)); console.warn(w); });

async function doInstall() {
  if (S.installing) return;
  S.installing = true;
  S.warnings = [];
  resetStages();
  $('uacBanner').hidden = true;
  go(4);
  try {
    await invoke('install_package');
  } catch (e) {
    const err = normErr(e, 'INSTALL_FAILED');
    if (err.code === 'NEED_ELEVATION') {
      S.installing = false;
      return requestElevation('install');
    }
    showError(err, 4, doInstall);
  } finally {
    S.installing = false;
  }
}

/**
 * 请求提权并由提权后的新实例续跑 action。
 * 成功时后端会退出当前进程（新实例接管）；UAC 被取消则返回错误。
 */
async function requestElevation(action) {
  const isInstall = action === 'install';
  const r = await dialog({
    title: '需要管理员权限', icon: 'lock', tone: 'warn', ok: '继续',
    text: (isInstall ? '当前系统需要管理员权限才能完成安装。' : '卸载需要管理员权限。') +
      '\n点击「继续」后系统会弹出确认窗口，请点击「是」，程序会以管理员身份自动继续。',
  });
  if (!r.ok) {
    if (isInstall) go(3); // 回到校验页，可再次点击安装
    return;
  }
  if (isInstall) {
    $('uacBanner').hidden = false;
    $('instTxt').textContent = '已请求管理员权限，请在系统弹窗中点击「是」…';
  } else {
    busy('已请求管理员权限，请在系统弹窗中点击「是」…');
  }
  try {
    await invoke('restart_elevated_cmd', { action });
    // 正常情况下当前进程已退出，不会走到这里
  } catch (e) {
    busy(false);
    const err = normErr(e);
    if (isInstall) showError(err, 4, doInstall);
    else toast(err.code === 'UAC_CANCELLED' ? '未获得管理员权限，已取消卸载' : err.message, 3000);
  }
}

/* ---------------- 5 完成 ---------------- */
function armAutoClose() {
  $('doneSub').textContent = S.warnings.length
    ? `ChatGPT 已为当前用户安装完成。另有 ${S.warnings.length} 项提示（如整机预置未成功），不影响使用，详见诊断日志。`
    : 'ChatGPT 已就绪，可以在开始菜单中找到它。';
  let n = 10;
  const tick = () => {
    if (S.cur !== 5) return;
    if (n <= 0) { invoke('exit_app'); return; }
    setStatus(`<span>${n} 秒后自动关闭 · 点击窗口任意处取消</span>`);
    n--;
    S.autoClose = setTimeout(tick, 1000);
  };
  tick();
}
function clearAutoClose() {
  if (S.autoClose) { clearTimeout(S.autoClose); S.autoClose = null; }
}
document.addEventListener('pointerdown', () => {
  if (S.cur === 5 && S.autoClose) { clearAutoClose(); setStatus(''); }
}, true);
async function launchApp() {
  try { await invoke('launch_app'); }
  catch (e) { toast(normErr(e).message, 3000); return; }
  invoke('exit_app');
}

/* ---------------- 6 错误页 ---------------- */
/**
 * @param e        后端 AppError 或任意异常
 * @param step     出错的步骤（步骤条标红）
 * @param retry    「重试」时执行的动作
 */
function showError(e, step, retry) {
  const err = normErr(e);
  S.failedStep = step;
  S.retry = retry || (() => go(step));
  $('errTitle').textContent = ['', '环境检测未通过', '下载遇到问题', '安全校验未通过', '安装遇到问题'][step] || '遇到问题';
  $('errCode').textContent = err.code;
  $('errMsg').textContent = err.message;
  $('errHint').textContent = err.hint || '';
  go(6);
}
async function copyLog() {
  try {
    const log = await invoke('get_last_log');
    await navigator.clipboard.writeText(log || '(暂无日志)');
    toast('诊断日志已复制，可粘贴到 Issue 中反馈');
  } catch (e) {
    toast('复制失败：' + normErr(e).message, 3000);
  }
}

/* ---------------- 卸载 ---------------- */
onEvent('uninstall-progress', p => busy(p.text));
async function doUninstall(removeData) {
  busy('正在卸载…');
  try {
    const warnings = await invoke('uninstall_package_cmd', { removeData });
    busy(false);
    await dialog({
      title: '卸载完成', icon: 'check', cancel: false,
      text: warnings && warnings.length
        ? `ChatGPT 已卸载，但有 ${warnings.length} 项清理未完全成功（通常不影响），详情可在诊断日志中查看。`
        : 'ChatGPT 已从本机卸载' + (removeData ? '，登录状态与本地数据已清除。' : '。'),
    });
    S.update = null;
    if (S.cur === 0) checkUpdate(true);
  } catch (e) {
    busy(false);
    const err = normErr(e);
    if (err.code === 'NEED_ELEVATION') return requestElevation(removeData ? 'uninstall-data' : 'uninstall');
    showError(err, 0, () => go(0));
  }
}
$('uninstallBtn').addEventListener('click', async () => {
  const r = await dialog({
    title: '卸载 ChatGPT 桌面版', tone: 'danger', ok: '卸载', okClass: 'danger',
    text: '将为本机所有用户移除 ChatGPT 桌面版，并清理整机预置与残留服务。',
    checkbox: '同时删除登录状态与本地数据（不可恢复）',
  });
  if (r.ok) doUninstall(r.checked);
});

/* ---------------- 启动 ---------------- */
if (!DEMO) {
  // 桌面应用手感：禁用网页右键菜单（可选中文本的区域除外）
  document.addEventListener('contextmenu', e => {
    if (!e.target.closest('.selectable, input, code')) e.preventDefault();
  });
}

(async function init() {
  go(0);
  try {
    const info = await invoke('app_info');
    if (info && info.version) $('sysreq').textContent += ` · v${info.version}`;
  } catch (_) { /* 忽略 */ }
  // 提权重启后：主动领取续跑动作（不依赖后端定时推送，避免前端未就绪时丢失）
  try {
    const action = await invoke('take_resume_action');
    if (action === 'install') doInstall();
    else if (action === 'uninstall' || action === 'uninstall-data') doUninstall(action === 'uninstall-data');
  } catch (_) { /* 忽略 */ }
})();
