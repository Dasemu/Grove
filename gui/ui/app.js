'use strict';
// ---- backend adapter: Tauri shell (invoke) or the localhost dev bridge (fetch) ----------------
const tauri = window.__TAURI__ && window.__TAURI__.core;
async function post(path, body) {
  const r = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Grove': '1' }, body: JSON.stringify(body) });
  if (!r.ok) throw new Error(r.status + ' ' + (await r.text()));
  return r.json();
}
// Backend failures never throw into the UI: they come back as {ok:false, ...}.
const safe = (p, fail) => p.catch((e) => fail(String((e && e.message) || e)));
const ipc = (req) => safe(tauri ? tauri.invoke('ipc', { req }) : post('/api/ipc', req), (m) => ({ ok: false, offline: true, error: m }));
const cli = (args, stdin = '') => safe(tauri ? tauri.invoke('cli', { args, stdin }) : post('/api/cli', { args, stdin }), (m) => ({ ok: false, output: m }));
async function getConfig() {
  if (tauri) return tauri.invoke('config');
  return (await fetch('/api/config')).json();
}

// ---- tiny DOM helper (text is always set via textContent, never innerHTML) --------------------
function h(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (v === true) e.setAttribute(k, '');
    else if (v !== false && v != null) e.setAttribute(k, v);
  }
  for (const c of kids.flat()) if (c != null && c !== false) e.append(c.nodeType ? c : document.createTextNode(c));
  return e;
}
const $ = (s) => document.querySelector(s);

// ---- state ---------------------------------------------------------------------------------
const S = { out: {}, branchName: {}, busy: false, status: null, offline: false, config: undefined, log: [], logFilter: '', lastStates: new Map() };
const stateName = (s) => (s.startsWith('error') ? 'error' : s);
const STATE_LABEL = { synced: 'Synced', dirty: 'Editing', ahead: 'Ahead', behind: 'Behind', behinddirty: 'Behind + editing', diverged: 'Diverged', error: 'Error' };
const PLAN = {
  synced: 'Nothing to do.',
  dirty: 'Snapshot the working changes and push the snapshot ref.',
  ahead: 'Nothing automatic: push your commits yourself.',
  behind: 'Fast-forward to the remote tip.',
  behinddirty: 'Keep your edits; adopt the remote tip only if the content is identical.',
  diverged: 'Nothing automatic: choose how to resolve it.',
  error: 'Retry with backoff; see the error.',
};
const short = (p) => p.split('/').slice(-2).join('/');
function ago(ts, now) {
  if (!ts) return 'never';
  const d = Math.max(0, (now || Date.now() / 1000) - ts);
  if (d < 5) return 'just now';
  if (d < 60) return Math.round(d) + 's ago';
  if (d < 3600) return Math.round(d / 60) + ' min ago';
  if (d < 86400) return Math.round(d / 3600) + ' h ago';
  return Math.round(d / 86400) + ' d ago';
}
function toast(msg, bad) {
  const t = h('div', { class: 'toast' + (bad ? ' bad' : ''), role: 'status' }, msg);
  $('#toasts').append(t);
  setTimeout(() => t.remove(), 5000);
}
function notify(title, body) {
  toast(title + ': ' + body, true);
  try {
    if ('Notification' in window && Notification.permission === 'granted') new Notification(title, { body });
  } catch (_) { /* notifications are optional */ }
}
const badge = (st) => h('span', { class: 'badge b-' + stateName(st) }, STATE_LABEL[stateName(st)] || st);

// ---- aggregate -------------------------------------------------------------------------------
function aggregate() {
  const s = S.status;
  if (S.offline || !s) return { cls: 'pill-muted', text: S.offline ? 'Daemon offline' : 'Connecting…', bad: 0 };
  let attention = 0, dirty = 0, total = 0;
  for (const r of s.repos) {
    if (r.last_error && !r.worktrees.some((w) => stateName(w.state) === 'error')) attention++;
    for (const w of r.worktrees) {
      total++;
      const n = stateName(w.state);
      if (n === 'diverged' || n === 'error') attention++;
      else if (n !== 'synced') dirty++;
    }
  }
  if (s.paused) return { cls: 'pill-muted', text: 'Paused', total, attention };
  if (attention) return { cls: 'pill-bad', text: attention + ' need' + (attention === 1 ? 's' : '') + ' attention', total, attention };
  if (dirty) return { cls: 'pill-warn', text: dirty + ' syncing', total, attention };
  return { cls: 'pill-ok', text: 'All synced', total, attention };
}
function renderTop() {
  const a = aggregate();
  $('#pill').className = 'pill ' + a.cls;
  $('#pill-text').textContent = a.text;
  const paused = S.status && S.status.paused;
  $('#btn-pause').textContent = paused ? 'Resume sync' : 'Pause sync';
  $('#btn-pause').disabled = S.offline;
  $('#btn-sync').disabled = S.offline || paused;
  const m = $('#machine');
  m.textContent = '';
  if (S.status) m.append('Machine ', h('b', {}, S.status.machine));
  document.title = (a.attention ? '(' + a.attention + ') ' : '') + 'Grove';
}

// ---- views ---------------------------------------------------------------------------------
const views = {};
views.repos = () => {
  const s = S.status;
  if (S.offline) return offline();
  if (!s) return h('p', { class: 'muted' }, 'Loading…');
  const a = aggregate();
  const stats = h('div', { class: 'grid2' },
    stat(s.repos.length, 'Repositories'),
    stat(a.total || 0, 'Worktrees'),
    stat(a.attention || 0, 'Need attention'));
  if (!s.repos.length) return h('div', {}, head('Repositories', 'Everything grove keeps in sync on this machine.'), h('div', { class: 'card empty' }, 'No repositories yet. Create one under “New repository” or clone existing ones with “Import”.'));
  const rows = s.repos.map((r) => h('tr', { class: 'row', tabindex: 0, onclick: () => (location.hash = '#/repo/' + encodeURIComponent(r.path)), onkeydown: (e) => { if (e.key === 'Enter') location.hash = '#/repo/' + encodeURIComponent(r.path); } },
    h('td', {}, h('div', { class: 'repo-name' }, short(r.path)), h('div', { class: 'path' }, r.path)),
    h('td', {}, r.worktrees.map((w) => h('span', { title: w.branch || '' }, badge(w.state), h('span', { class: 'muted' }, (w.branch || '(detached)') + '  ')))),
    h('td', {}, h('span', { class: 'muted' }, r.phase)),
    h('td', {}, h('span', { class: 'muted' }, ago(r.last_sync, s.now)), r.last_error && h('div', { class: 'err' }, r.last_error))));
  return h('div', {}, head('Repositories', 'Everything grove keeps in sync on this machine.'), stats,
    h('div', { class: 'card' }, h('table', { class: 'table' },
      h('thead', {}, h('tr', {}, h('th', {}, 'Repository'), h('th', {}, 'Worktrees'), h('th', {}, 'Phase'), h('th', {}, 'Last sync'))),
      h('tbody', {}, rows))));
};
const stat = (n, label) => h('div', { class: 'card stat' }, h('b', {}, String(n)), h('span', {}, label));
const head = (t, sub) => h('div', {}, h('h1', {}, t), h('p', { class: 'sub' }, sub));
const offline = () => h('div', { class: 'card empty' }, h('b', {}, 'The grove daemon is not running.'), h('p', {}, 'Start it with ', h('code', {}, 'systemctl --user start groved'), ' (or run ', h('code', {}, 'groved'), ' in a terminal).'));

views.repo = (path) => {
  const r = S.status && S.status.repos.find((x) => x.path === path);
  if (S.offline) return offline();
  if (!r) return h('div', {}, h('a', { class: 'back', href: '#/repos' }, '← Repositories'), h('p', { class: 'muted' }, 'Repository not found.'));
  const rows = r.worktrees.map((w) => {
    const n = stateName(w.state);
    return h('tr', {},
      h('td', {}, h('div', { class: 'repo-name' }, w.branch || '(detached)'), h('div', { class: 'path' }, w.path || '')),
      h('td', {}, badge(w.state)),
      h('td', { class: 'muted' }, PLAN[n] || ''));
  });
  const diverged = r.worktrees.filter((w) => stateName(w.state) === 'diverged');
  return h('div', {},
    h('a', { class: 'back', href: '#/repos' }, '← Repositories'),
    h('h1', {}, short(r.path)),
    h('p', { class: 'sub path' }, r.path),
    h('div', { class: 'actions' }, h('button', { class: 'btn', onclick: () => syncRepo(r.path) }, 'Sync this repository')),
    r.last_error && h('div', { class: 'err' }, r.last_error),
    h('h2', {}, 'Worktrees'),
    h('div', { class: 'card' }, h('table', { class: 'table' },
      h('thead', {}, h('tr', {}, h('th', {}, 'Branch'), h('th', {}, 'State'), h('th', {}, 'Next sync pass'))), h('tbody', {}, rows))),
    diverged.map((w) => resolvePanel(w)),
    r.worktrees.filter((w) => stateName(w.state) !== 'diverged' && S.out[w.path]).map((w) => h('div', { class: 'term' + (S.out[w.path].ok ? '' : ' fail') }, S.out[w.path].text)));
};
function resolvePanel(w) {
  const out = h('div');
  const run = async (flag, label) => {
    if (!confirm(label + '\n\nA backup ref is written first, so nothing is lost. Continue?')) return;
    const r = await cli(['resolve', w.path, ...flag]);
    S.out[w.path] = { ok: r.ok, text: r.output };
    render(true);
    refresh();
  };
  if (!w.path) return h('div', { class: 'callout' }, 'Diverged, but this daemon does not report the worktree path. Update groved.');
  const prev = S.out[w.path];
  if (prev) out.append(h('div', { class: 'term' + (prev.ok ? '' : ' fail') }, prev.text));
  const branchName = h('input', { type: 'text', id: 'brname', placeholder: 'branch name', 'aria-label': 'New branch name', value: S.branchName[w.path] ?? (w.branch || 'work') + '-local', oninput: (e) => { S.branchName[w.path] = e.target.value; } });
  return h('div', { class: 'callout' },
    h('h3', {}, 'Diverged: ' + (w.branch || '') + ' has commits here and on the remote'),
    h('p', {}, 'Grove never merges for you. Pick what should win:'),
    h('div', { class: 'actions' },
      h('button', { class: 'btn', onclick: () => run(['--save-as', branchName.value], 'Keep your commits on branch “' + branchName.value + '” and move this branch to the remote tip') }, 'Save mine as a branch'), branchName,
      h('button', { class: 'btn', onclick: () => run(['--take-remote'], 'Discard your local commits and take the remote tip') }, 'Take remote'),
      h('button', { class: 'btn btn-danger', onclick: () => run(['--keep-local'], 'Overwrite the remote branch with your local commits') }, 'Keep mine (overwrite remote)')),
    h('p', { class: 'muted' }, 'To inspect both sides first, run ', h('code', {}, 'grove resolve ' + w.path + ' --compare'), ' in a terminal.'),
    out);
}

views.activity = () => {
  if (S.offline) return offline();
  const f = S.logFilter.toLowerCase();
  const items = S.log.slice().reverse().filter((e) => !f || (e.kind + ' ' + (e.repo || '') + ' ' + e.msg).toLowerCase().includes(f));
  const filter = h('input', { type: 'text', id: 'flt', placeholder: 'Filter…', 'aria-label': 'Filter activity', value: S.logFilter, oninput: (e) => { S.logFilter = e.target.value; render(true); } });
  const body = items.length
    ? h('table', { class: 'table' }, h('thead', {}, h('tr', {}, h('th', {}, 'Time'), h('th', {}, 'Event'), h('th', {}, 'Repository'), h('th', {}, 'Details'))),
      h('tbody', {}, items.map((e) => h('tr', {},
        h('td', { class: 'muted' }, new Date(e.ts * 1000).toLocaleTimeString('en-GB')),
        h('td', {}, h('span', { class: 'kind k-' + e.kind }, e.kind)),
        h('td', { class: 'muted' }, e.repo || '–'),
        h('td', {}, e.msg)))))
    : h('div', { class: 'empty' }, 'No activity yet.');
  return h('div', {}, head('Activity', 'What the daemon did, newest first.'), h('div', { class: 'actions' }, filter), h('div', { class: 'card', style: 'margin-top:12px' }, body));
};

function groupSelect() {
  const groups = (S.config && S.config.groups) || [];
  return h('select', { id: 'group', 'aria-label': 'Group' }, groups.map((g) => h('option', { value: g.name }, g.name + ' → ' + g.primary)));
}
function needConfig() {
  return S.config && S.config.exists ? null : h('div', { class: 'card empty' }, 'Finish onboarding first (Settings).');
}
views.new = () => {
  if (needConfig()) return needConfig();
  const out = h('div');
  const name = h('input', { type: 'text', id: 'name', placeholder: 'my-project', pattern: '[A-Za-z0-9._-]+' });
  const pub = h('input', { type: 'checkbox', id: 'public' });
  const btn = h('button', { class: 'btn btn-primary', type: 'submit' }, 'Create repository');
  const form = h('form', { class: 'card form', onsubmit: async (e) => {
    e.preventDefault();
    if (!name.value.trim()) return;
    btn.disabled = true; btn.textContent = 'Creating…';
    const args = ['new', $('#group').value, name.value.trim()].concat(pub.checked ? ['--public'] : []);
    const r = await cli(args);
    out.replaceChildren(h('div', { class: 'term' + (r.ok ? '' : ' fail') }, r.output));
    btn.disabled = false; btn.textContent = 'Create repository';
    if (r.ok) { toast('Repository created'); name.value = ''; refresh(); }
  } },
    h('label', {}, 'Group', groupSelect()),
    h('label', {}, 'Name', name, h('span', { class: 'hint' }, 'Created on the group’s primary forge, with an initial commit and the bare + worktrees layout.')),
    h('label', { class: 'check' }, pub, 'Public repository'), btn);
  return h('div', {}, head('New repository', 'Creates the remote, the local checkout, the mirrors and registers it for your other machines.'), form, out);
};

views.import = () => {
  if (needConfig()) return needConfig();
  const out = h('div'), list = h('div');
  const sel = groupSelect();
  const scan = async () => {
    list.replaceChildren(h('p', { class: 'muted' }, 'Scanning…'));
    out.replaceChildren();
    const r = await cli(['import', sel.value]);
    const names = (r.output.match(/^\s+\S+\/(\S+?)(?: on \S+)?(?: \(private\))?$/gm) || []).map((l) => l.trim().split(/\s+/)[0].split('/')[1]);
    if (!r.ok) return list.replaceChildren(h('div', { class: 'term fail' }, r.output));
    if (!names.length) return list.replaceChildren(h('p', { class: 'muted' }, 'Nothing to import: every remote repository is already on this machine.'));
    const boxes = names.map((n) => h('label', { class: 'check' }, h('input', { type: 'checkbox', value: n }), n));
    const go = h('button', { class: 'btn btn-primary', onclick: async () => {
      const picked = [...list.querySelectorAll('input:checked')].map((i) => i.value);
      if (!picked.length) return;
      go.disabled = true;
      const res = await cli(['import', sel.value, ...picked]);
      out.replaceChildren(h('div', { class: 'term' + (res.ok ? '' : ' fail') }, res.output));
      go.disabled = false; refresh();
      if (res.ok) scan();
    } }, 'Import selected');
    list.replaceChildren(h('div', { class: 'card form' }, boxes, go));
  };
  return h('div', {}, head('Import', 'Clone repositories that exist on your forge but not on this machine.'),
    h('div', { class: 'actions' }, sel, h('button', { class: 'btn', onclick: scan }, 'Scan remote')), list, out);
};

views.settings = () => {
  const c = S.config;
  if (!c || !c.exists) return onboarding();
  const cfg = c.config;
  const kv = (rows) => h('dl', { class: 'kv card' }, rows.flatMap(([k, v]) => [h('dt', {}, k), h('dd', {}, v)]));
  const provs = Object.entries(cfg.providers || {});
  const out = h('div');
  const prov = h('select', { id: 'tokprov', 'aria-label': 'Provider' }, provs.map(([id]) => h('option', { value: id }, id)));
  const tok = h('input', { type: 'password', id: 'tok', autocomplete: 'off', placeholder: 'API token' });
  const act = async (args, stdin) => { const r = await cli(args, stdin); out.replaceChildren(h('div', { class: 'term' + (r.ok ? '' : ' fail') }, r.output)); refresh(); };
  return h('div', {}, head('Settings', 'Read from config.toml. Tokens live in the system keyring, never in the file.'),
    h('h2', {}, 'This machine'),
    kv([['Name', cfg.machine.name], ['Workspace root', cfg.root.path], ['Fetch interval', ((cfg.sync && cfg.sync.fetch_interval_s) || 60) + ' s'], ['Largest synced file', ((cfg.sync && cfg.sync.max_file_mb) ?? 50) + ' MiB']]),
    h('h2', {}, 'Providers'),
    kv(provs.map(([id, p]) => [id, p.kind + (p.url ? ' · ' + p.url : '')])),
    h('h2', {}, 'Groups'),
    kv((cfg.groups || []).map((g) => [g.name, 'primary ' + g.primary + (g.mirror_to && g.mirror_to.length ? ' · mirrors: ' + g.mirror_to.join(', ') : '') + (g.public ? ' · public: ' + g.public.primary + ((g.public.mirror_to || []).length ? ' → ' + g.public.mirror_to.join(', ') : '') : '') + ' · ' + (g.layout || 'bare-worktrees')])),
    h('h2', {}, 'Config repository'),
    kv([['Repository', cfg.config_repo ? cfg.config_repo.name + ' on ' + cfg.config_repo.provider : 'not configured']]),
    h('div', { class: 'actions' },
      h('button', { class: 'btn', onclick: () => act(['config-repo-init']) }, 'Register repos on disk'),
      h('button', { class: 'btn', onclick: () => act(['bootstrap']) }, 'Bootstrap from config repo')),
    h('h2', {}, 'Sign in to a provider'),
    h('form', { class: 'card form', onsubmit: (e) => { e.preventDefault(); if (tok.value) { act(['login', prov.value], tok.value + '\n'); tok.value = ''; } } },
      h('label', {}, 'Provider', prov), h('label', {}, 'Token', tok, h('span', { class: 'hint' }, 'Needs repository read/write scope. Sent to the forge only.')),
      h('button', { class: 'btn btn-primary', type: 'submit' }, 'Verify and store')),
    h('h2', {}, 'Encrypted files (.env)'),
    h('div', { class: 'actions' },
      h('button', { class: 'btn', onclick: () => act(['secrets', 'init']) }, 'Create this machine’s key'),
      h('button', { class: 'btn', onclick: () => act(['secrets', 'pull']) }, 'Pull secrets'),
      h('button', { class: 'btn', onclick: () => act(['secrets', 'rekey']) }, 'Re-encrypt for all machines')),
    out);
};
function onboarding() {
  const f = {};
  const field = (id, label, value, hint) => { f[id] = h('input', { type: 'text', id, value }); return h('label', {}, label, f[id], hint && h('span', { class: 'hint' }, hint)); };
  f.kind = h('select', { id: 'kind' }, ['gitea', 'forgejo', 'github'].map((k) => h('option', { value: k }, k)));
  f.token = h('input', { type: 'password', id: 'token', autocomplete: 'off' });
  const out = h('div');
  const btn = h('button', { class: 'btn btn-primary', type: 'submit' }, 'Set up grove');
  const form = h('form', { class: 'card form', onsubmit: async (e) => {
    e.preventDefault(); btn.disabled = true;
    const lines = [];
    const step = async (args, stdin) => { const r = await cli(args, stdin); lines.push('$ grove ' + args[0] + '\n' + r.output); out.replaceChildren(h('div', { class: 'term' + (r.ok ? '' : ' fail') }, lines.join('\n\n'))); return r.ok; };
    const a = ['init', '--machine', f.machine.value, '--root', f.root.value, '--provider', f.provider.value, '--kind', f.kind.value, '--group', f.group.value, '--config-repo', f.crepo.value];
    if (f.url.value) a.push('--url', f.url.value);
    const existing = await getConfig(); // a previous attempt may already have written the config
    if ((existing.exists || await step(a)) && await step(['login', f.provider.value], f.token.value + '\n') && await step(['bootstrap'])) { toast('All set'); S.config = undefined; await refresh(); location.hash = '#/repos'; }
    btn.disabled = false;
  } },
    field('machine', 'Machine name', 'my-laptop', 'Letters, digits, - and _. Unique per computer.'),
    field('root', 'Workspace folder', '~/Workspace'),
    h('label', {}, 'Forge type', f.kind),
    field('provider', 'Provider id', 'main'),
    field('url', 'Forge URL', 'https://git.example.com', 'Empty for github.com.'),
    field('group', 'First group', 'work'),
    field('crepo', 'Config repository', 'grove-config', 'Private repo holding the list of your repos, so a new machine only needs to sign in.'),
    h('label', {}, 'API token', f.token), btn);
  return h('div', {}, head('Welcome to Grove', 'Set up this machine. On a second computer this is all you do: sign in and your workspace appears.'), form, out);
}

// ---- router / render -------------------------------------------------------------------------
function route() {
  const hash = location.hash.replace(/^#\/?/, '') || 'repos';
  const [name, ...rest] = hash.split('/');
  return { name, arg: rest.length ? decodeURIComponent(rest.join('/')) : null };
}
let lastKey = '';
function render(force) {
  const { name, arg } = route();
  document.querySelectorAll('#nav a').forEach((a) => a.classList.toggle('active', a.dataset.route === (name === 'repo' ? 'repos' : name)));
  renderTop();
  // Avoid clobbering forms while the user types: re-render form views only on navigation.
  const formView = ['new', 'import', 'settings'].includes(name);
  const key = name + '|' + arg;
  if (!force && formView && key === lastKey) return;
  lastKey = key;
  // Never rebuild under the user's cursor on a background poll.
  const ae = document.activeElement;
  const typing = ae && $('#view').contains(ae) && /^(INPUT|SELECT|TEXTAREA)$/.test(ae.tagName);
  if (!force && typing) return;
  const keep = typing && ae.id ? { id: ae.id, pos: ae.selectionStart } : null;
  const v = name === 'repo' ? views.repo(arg) : (views[name] || views.repos)();
  $('#view').replaceChildren(v);
  if (keep) { const e = document.getElementById(keep.id); if (e) { e.focus(); try { e.setSelectionRange(keep.pos, keep.pos); } catch (_) { /* not a text input */ } } }
}

// ---- actions & polling -----------------------------------------------------------------------
async function syncRepo(path) {
  const r = await ipc({ v: 1, cmd: 'sync', repo: path });
  toast(r.ok ? 'Sync queued' : r.error, !r.ok);
}
$('#btn-sync').addEventListener('click', async () => { const r = await ipc({ v: 1, cmd: 'sync' }); toast(r.ok ? 'Sync queued for all repositories' : r.error, !r.ok); });
$('#btn-pause').addEventListener('click', async () => {
  const paused = S.status && S.status.paused;
  await ipc({ v: 1, cmd: paused ? 'resume' : 'pause' });
  refresh();
});
function watchTransitions() {
  if (!S.status) return;
  for (const r of S.status.repos) for (const w of r.worktrees) {
    const key = w.path || r.path + w.branch, n = stateName(w.state), prev = S.lastStates.get(key);
    if (prev && prev !== n && (n === 'diverged' || n === 'error')) notify(n === 'diverged' ? 'Diverged' : 'Sync error', short(r.path) + ' · ' + (w.branch || ''));
    S.lastStates.set(key, n);
  }
}
async function refresh() {
  if (S.busy) return;
  S.busy = true;
  try {
    const st = await ipc({ v: 1, cmd: 'status' });
    S.offline = !st.ok; S.status = st.ok ? st.data : null;
    if (st.ok) { const lg = await ipc({ v: 1, cmd: 'log', limit: 200 }); if (lg.ok) S.log = lg.data; }
  } catch (_) { S.offline = true; S.status = null; }
  if (S.config === undefined) { try { S.config = await getConfig(); } catch (_) { S.config = { exists: false }; } }
  try { watchTransitions(); } finally { S.busy = false; }
  render(false);
}
window.addEventListener('hashchange', () => render(true));
refresh().then(() => { if (!(S.config && S.config.exists) && !location.hash) location.hash = '#/settings'; });
setInterval(refresh, 2000); // refresh() skips while a previous one is still in flight
