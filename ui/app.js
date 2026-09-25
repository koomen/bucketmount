// BucketMount front end. Plain JS talking to the Rust core through Tauri.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const PROVIDERS = ["AWS", "Cloudflare", "DigitalOcean", "Minio", "Wasabi", "Backblaze", "GCS", "Scaleway", "Other"];
const $app = document.getElementById("app");
const $modal = document.getElementById("modal");
const $toast = document.getElementById("toast");

let snap = null;
// view: { kind: "list" } | { kind: "edit", original: string|null, draft: MountConfig, cred: "keys"|"env"|"remote" }
let view = { kind: "list" };
let mountPointAuto = false;
let toastTimer = null;

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

function toast(msg, isError = false) {
  $toast.textContent = msg;
  $toast.className = "toast" + (isError ? " error" : "");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => $toast.classList.add("hidden"), isError ? 6000 : 2500);
}

async function refresh() {
  snap = await invoke("snapshot");
  render();
}

function render() {
  if (!snap) return;
  renderSignInBanner();
  if (view.kind === "list") renderList();
  else renderEditorStatus();
  renderModal();
}

// ------------------------------------------------------------------ AWS SSO sign-in

async function signIn(mount) {
  try { await invoke("sso_login", { mount }); }
  catch (err) { toast(String(err), true); }
}

// A fixed strip shown on every view while a sign-in waits for the browser.
function renderSignInBanner() {
  let el = document.getElementById("signin");
  const l = snap.sso_login;
  document.body.classList.toggle("has-signin", !!l);
  if (!l) { if (el) el.remove(); return; }
  if (!el) {
    el = document.createElement("div");
    el.id = "signin";
    el.className = "signin-banner";
    document.body.appendChild(el);
  }
  el.innerHTML = l.user_code
    ? `<span class="spinner"></span><span class="grow">Approve the sign-in for AWS profile <b>${esc(l.profile)}</b> in your browser. Check that it shows code <b class="code">${esc(l.user_code)}</b>.</span>
       <button class="btn btn-sm" id="signin-open">Open page again</button><button class="btn btn-sm" id="signin-cancel">Cancel</button>`
    : `<span class="spinner"></span><span class="grow">Starting AWS sign-in for profile <b>${esc(l.profile)}</b>…</span><button class="btn btn-sm" id="signin-cancel">Cancel</button>`;
  const open = el.querySelector("#signin-open");
  if (open) open.onclick = () => {
    const m = snap.mounts.find((x) => x.sso_profile === l.profile);
    if (m) signIn(m.config);
  };
  el.querySelector("#signin-cancel").onclick = () => invoke("cancel_sso_login");
}

// ------------------------------------------------------------------ list

function summaryText() {
  const active = snap.mounts.filter((m) => m.state !== "disabled");
  if (!snap.mounts.length) return "No mounts configured";
  if (!active.length) return "All mounts disabled";
  const problems = active.filter((m) => ["disconnected", "sign_in_required", "down", "error"].includes(m.state)).length;
  if (problems) return `${problems} of ${active.length} mount${active.length > 1 ? "s" : ""} need attention`;
  if (active.some((m) => m.state === "syncing")) return "Syncing";
  if (active.some((m) => m.state === "starting")) return "Starting…";
  return `${active.length} mount${active.length > 1 ? "s" : ""} connected`;
}

function renderList() {
  const cards = snap.mounts.map((m) => {
    const problem = ["disconnected", "sign_in_required", "down", "error"].includes(m.state);
    const detail = m.detail && m.detail !== m.state_label ? `<div class="detail ${problem ? "problem" : ""}">${esc(m.detail)}${m.restarts ? ` <span class="tiny">· ${m.restarts} restart${m.restarts > 1 ? "s" : ""}</span>` : ""}</div>` : "";
    const path = m.config.prefix ? `${m.config.bucket}/${m.config.prefix}` : m.config.bucket;
    return `
      <div class="card">
        <span class="dot" style="background:${m.color}"></span>
        <div>
          <div class="title"><span class="name">${esc(m.config.name)}</span><span class="state" style="color:${m.color}">${esc(m.state_label)}</span></div>
          <div class="meta"><span title="s3://${esc(path)}">s3://${esc(path)}</span><span title="${esc(m.mount_path)}">${esc(m.mount_path_short)}</span></div>
          ${detail}
        </div>
        <div class="actions">
          ${m.state === "sign_in_required" ? `<button class="btn btn-primary" data-signin="${esc(m.config.name)}">Sign in</button>` : ""}
          <button class="btn" data-open="${esc(m.config.name)}" ${m.mounted ? "" : "disabled"}>Open</button>
          <button class="btn" data-edit="${esc(m.config.name)}">Edit</button>
        </div>
      </div>`;
  });

  const body = snap.mounts.length
    ? `<div class="cards">${cards.join("")}</div>`
    : `<div class="empty"><div class="big">No mounts yet</div>Click “Add mount” to connect an S3 bucket. It will appear as a volume in Finder.</div>`;

  const rcloneLine = snap.rclone_path
    ? `<span class="tiny grow">${esc(snap.rclone_version || "rclone")} · ${esc(snap.rclone_path)}</span>`
    : `<span class="grow" style="color:var(--danger)">rclone was not found. Use the downloadable BucketMount.app, or set rclone_path in config.toml.</span>`;

  $app.innerHTML = `
    <div class="header">
      <div><h1>Mounts</h1><div class="summary">${esc(summaryText())}</div></div>
      <button class="btn btn-primary" id="add">Add mount</button>
    </div>
    ${snap.config_error ? `<div class="banner">Config file could not be read: ${esc(snap.config_error)}</div>` : ""}
    ${body}
    <div class="footer">
      <div class="row">
        <label class="switch"><input type="checkbox" id="login" ${snap.start_at_login ? "checked" : ""}><span class="knob"></span></label>
        <span>Start BucketMount at login</span>
      </div>
      <div class="row">${rcloneLine}</div>
      <div class="row"><span class="tiny grow">Config: ${esc(snap.config_path)}</span><button class="btn btn-sm" id="reveal-config">Reveal</button></div>
      <div class="row"><span class="tiny grow">Logs: ${esc(snap.logs_dir)}</span><button class="btn btn-sm" id="open-logs">Open</button></div>
      <div class="row"><span class="tiny grow">BucketMount ${esc(snap.version)} · updates install automatically</span><button class="btn btn-sm" id="check-updates">Check for updates</button><button class="btn btn-sm" id="quit">Quit BucketMount</button></div>
    </div>`;

  $app.querySelector("#add").onclick = () => openEditor(null);
  $app.querySelectorAll("[data-open]").forEach((b) => (b.onclick = () => invoke("open_mount", { name: b.dataset.open })));
  $app.querySelectorAll("[data-edit]").forEach((b) => (b.onclick = () => openEditor(b.dataset.edit)));
  $app.querySelectorAll("[data-signin]").forEach((b) => (b.onclick = () => {
    const m = snap.mounts.find((x) => x.config.name === b.dataset.signin);
    if (m) signIn(m.config);
  }));
  $app.querySelector("#login").onchange = async (e) => {
    try { await invoke("set_start_at_login", { enabled: e.target.checked }); }
    catch (err) { toast(String(err), true); e.target.checked = !e.target.checked; }
  };
  $app.querySelector("#reveal-config").onclick = () => invoke("reveal_config");
  $app.querySelector("#open-logs").onclick = () => invoke("open_logs");
  $app.querySelector("#check-updates").onclick = () => { invoke("check_for_updates"); toast("Checking for updates…"); };
  $app.querySelector("#quit").onclick = () => invoke("quit");
}

// ------------------------------------------------------------------ editor

function credModeOf(m) {
  if (m.rclone_remote) return "remote";
  if (m.access_key_id) return "keys";
  return "env";
}

function newMount() {
  return {
    name: "", mode: "sync", bucket: "", prefix: "", mount_point: "", enabled: true, read_only: false,
    provider: "AWS", region: "us-east-1", endpoint: "", access_key_id: "", secret_access_key: "",
    rclone_remote: "", env_auth: false, aws_profile: "", write_back_secs: 5, dir_cache_secs: 60, cache_max_size: "10G", extra_args: [],
  };
}

function openEditor(name) {
  const existing = name ? snap.mounts.find((m) => m.config.name === name) : null;
  const draft = existing ? structuredClone(existing.config) : newMount();
  mountPointAuto = !draft.mount_point;
  view = { kind: "edit", original: existing ? name : null, draft, cred: credModeOf(draft), testing: false, testResult: null, error: null, confirmDelete: false };
  renderEditor();
}

function field(id, label, value, opts = {}) {
  const type = opts.password ? "password" : "text";
  return `<label for="${id}">${label}</label><input id="${id}" type="${type}" value="${esc(value)}" placeholder="${esc(opts.placeholder || "")}" spellcheck="false" autocapitalize="off" autocorrect="off">`;
}

function renderEditor() {
  const v = view;
  const d = v.draft;
  const isNew = v.original === null;
  const sync = d.mode === "sync";
  const credFields = {
    keys: `${field("access_key_id", "Access key ID", d.access_key_id)}
           <label for="secret">Secret access key</label>
           <div class="inline"><input id="secret" type="password" value="${esc(d.secret_access_key)}" spellcheck="false"><button class="btn btn-sm" id="toggle-secret" type="button">Show</button></div>`,
    env: `${field("aws_profile", "AWS profile", d.aws_profile, { placeholder: "default" })}
          <div class="full hint tiny">A profile from ~/.aws/config or ~/.aws/credentials. For SSO profiles BucketMount signs you in when the session expires. Leave empty for the default profile, environment variables or an instance role.</div>`,
    remote: `${field("rclone_remote", "Remote name", d.rclone_remote, { placeholder: "s3" })}
             <div class="full hint tiny">A remote from ~/.config/rclone/rclone.conf. Provider, region and endpoint above are ignored.</div>
             ${field("aws_profile", "AWS profile", d.aws_profile, { placeholder: "from the remote" })}
             <div class="full hint tiny">Optional: overrides the remote's profile. For SSO profiles BucketMount signs you in when the session expires.</div>`,
  }[v.cred];

  $app.innerHTML = `
    <div class="editor-head">
      <button class="btn" id="back">‹ Back</button>
      <h1>${isNew ? "New mount" : "Edit mount"}</h1>
    </div>

    <h2>Bucket</h2>
    <div class="form">
      <label>Mode</label>
      <div class="segmented" id="mode">
        <button type="button" data-mode="sync" class="${sync ? "active" : ""}">Synced folder</button>
        <button type="button" data-mode="mount" class="${sync ? "" : "active"}">Mount</button>
      </div>
      <div class="full hint tiny">${sync
        ? "A normal folder on this Mac, kept in two-way sync with the bucket. Works offline and behaves like any local folder; needs disk space for the whole bucket."
        : "The bucket as a network volume. Files are downloaded when opened, so it needs no disk space, but every save goes over the network."}</div>
      ${field("name", "Name", d.name, { placeholder: "my-bucket" })}
      <div class="full hint tiny">${sync ? "Must be unique." : "Shown as the volume name. Must be unique."}</div>
      ${field("bucket", "Bucket", d.bucket, { placeholder: "bucket-name" })}
      ${field("prefix", "Path in bucket", d.prefix, { placeholder: "optional/sub/folder" })}
      ${field("mount_point", sync ? "Local folder" : "Mount point", d.mount_point, { placeholder: "~/BucketMount/name" })}
      <div class="full hint tiny">${sync
        ? "Created if missing. Files already in it are merged with the bucket on the first sync."
        : "An empty folder. While mounted it appears in the Finder sidebar under Locations."}</div>
    </div>

    <h2>Connection</h2>
    <div class="form">
      <label for="provider">Provider</label>
      <select id="provider">${PROVIDERS.map((p) => `<option ${p === d.provider ? "selected" : ""}>${p}</option>`).join("")}</select>
      ${field("region", "Region", d.region, { placeholder: "us-east-1" })}
      <div id="endpoint-row" class="${d.provider === "AWS" ? "hidden" : ""}" style="display:contents">
        ${field("endpoint", "Endpoint", d.endpoint, { placeholder: "https://…" })}
      </div>
    </div>

    <h2>Credentials</h2>
    <div class="form">
      <label>Source</label>
      <div class="segmented" id="cred">
        <button type="button" data-cred="keys" class="${v.cred === "keys" ? "active" : ""}">Access keys</button>
        <button type="button" data-cred="env" class="${v.cred === "env" ? "active" : ""}">AWS default chain</button>
        <button type="button" data-cred="remote" class="${v.cred === "remote" ? "active" : ""}">rclone remote</button>
      </div>
      ${credFields}
    </div>

    <h2>Options</h2>
    <div class="form">
      <label>Behaviour</label>
      <div class="checks">
        <label><input type="checkbox" id="enabled" ${d.enabled ? "checked" : ""}> Enabled (${sync ? "sync" : "mount"} automatically)</label>
        <label class="${sync ? "hidden" : ""}"><input type="checkbox" id="read_only" ${d.read_only && !sync ? "checked" : ""}> Read only</label>
      </div>
    </div>

    <details>
      <summary>Advanced</summary>
      <div class="form">
        <label for="write_back_secs">Upload delay</label>
        <div class="inline"><input class="short" id="write_back_secs" type="number" min="1" max="600" value="${d.write_back_secs}"><span class="tiny">${sync ? "seconds after the last local change before syncing" : "seconds after the last write before a file is uploaded"}</span></div>
        <label for="dir_cache_secs">${sync ? "Bucket check" : "Listing cache"}</label>
        <div class="inline"><input class="short" id="dir_cache_secs" type="number" min="1" max="3600" value="${d.dir_cache_secs}"><span class="tiny">${sync ? "seconds between checks for changes made elsewhere (min 10)" : "seconds until changes made elsewhere appear"}</span></div>
        <div style="display:contents" class="${sync ? "hidden" : ""}">
          <label for="cache_max_size">Local cache limit</label>
          <div class="inline"><input class="short" id="cache_max_size" type="text" value="${esc(d.cache_max_size)}"><span class="tiny">e.g. 10G</span></div>
        </div>
        ${field("extra_args", "Extra rclone flags", (d.extra_args || []).join(" "), { placeholder: "--transfers 8" })}
      </div>
    </details>

    <div class="error ${v.error ? "" : "hidden"}" id="error">${esc(v.error || "")}</div>

    <div class="actions-row">
      <button class="btn btn-primary" id="save">Save</button>
      <button class="btn" id="cancel">Cancel</button>
      <button class="btn" id="test" ${v.testing ? "disabled" : ""}>Test connection</button>
      <span id="test-result" class="test-result"></span>
      <span class="spacer"></span>
      ${isNew ? "" : `<span id="delete-area"><button class="btn btn-danger" id="delete">Delete…</button></span>`}
    </div>

    ${isNew ? "" : `<div class="status-panel" id="status-panel"></div>`}`;

  const read = () => {
    const g = (id) => $app.querySelector("#" + id);
    d.name = g("name").value;
    d.bucket = g("bucket").value;
    d.prefix = g("prefix").value;
    d.mount_point = g("mount_point").value;
    d.provider = g("provider").value;
    d.region = g("region").value;
    d.endpoint = g("endpoint") ? g("endpoint").value : d.endpoint;
    d.enabled = g("enabled").checked;
    d.read_only = d.mode !== "sync" && g("read_only").checked;
    d.write_back_secs = Math.max(1, parseInt(g("write_back_secs").value || "5", 10));
    d.dir_cache_secs = Math.max(1, parseInt(g("dir_cache_secs").value || "60", 10));
    d.cache_max_size = g("cache_max_size").value || "10G";
    d.extra_args = g("extra_args").value.split(/\s+/).filter(Boolean);
    // Only pick up the visible credential fields: values typed under the
    // other sources survive switching back and forth until save.
    if (v.cred === "keys") {
      d.access_key_id = g("access_key_id").value;
      d.secret_access_key = g("secret").value;
    } else {
      if (v.cred === "remote") d.rclone_remote = g("rclone_remote").value;
      d.aws_profile = g("aws_profile").value.trim();
    }
    return d;
  };

  // The mount as it will be saved or tested: only the chosen source's fields.
  const forSave = () => {
    const m = structuredClone(read());
    if (v.cred !== "keys") { m.access_key_id = ""; m.secret_access_key = ""; }
    if (v.cred !== "remote") m.rclone_remote = "";
    if (v.cred === "keys") m.aws_profile = "";
    m.env_auth = v.cred === "env";
    return m;
  };

  const q = (sel) => $app.querySelector(sel);
  q("#back").onclick = q("#cancel").onclick = () => { view = { kind: "list" }; render(); };
  q("#name").oninput = async (e) => {
    if (mountPointAuto) q("#mount_point").value = await invoke("default_mount_point", { name: e.target.value });
  };
  q("#mount_point").oninput = () => { mountPointAuto = false; };
  if (mountPointAuto && !q("#mount_point").value && q("#name").value) q("#name").dispatchEvent(new Event("input"));
  q("#provider").onchange = (e) => { q("#endpoint-row").classList.toggle("hidden", e.target.value === "AWS"); };
  q("#mode").querySelectorAll("button").forEach((b) => (b.onclick = () => { read(); d.mode = b.dataset.mode; renderEditor(); }));
  q("#cred").querySelectorAll("button").forEach((b) => (b.onclick = () => { read(); v.cred = b.dataset.cred; renderEditor(); }));
  const ts = q("#toggle-secret");
  if (ts) ts.onclick = () => { const s = q("#secret"); s.type = s.type === "password" ? "text" : "password"; ts.textContent = s.type === "password" ? "Show" : "Hide"; };

  q("#save").onclick = async () => {
    try {
      await invoke("save_mount", { mount: forSave(), originalName: v.original });
      view = { kind: "list" };
      toast("Saved");
      await refresh();
    } catch (err) {
      v.error = String(err);
      q("#error").textContent = v.error; q("#error").classList.remove("hidden");
    }
  };
  q("#test").onclick = async () => {
    const r = q("#test-result");
    q("#test").disabled = true;
    r.className = "test-result"; r.innerHTML = `<span class="spinner"></span> Checking…`;
    try {
      const n = await invoke("test_connection", { mount: forSave() });
      r.className = "test-result ok"; r.textContent = `✓ Bucket reachable (${n} top-level entr${n === 1 ? "y" : "ies"})`;
    } catch (err) {
      r.className = "test-result bad"; r.textContent = `✕ ${err}`;
      if (String(err).includes("SSO session expired")) {
        const b = document.createElement("button");
        b.className = "btn btn-sm btn-primary"; b.textContent = "Sign in"; b.style.marginLeft = "8px";
        const draft = forSave();
        b.onclick = () => signIn(draft);
        r.appendChild(b);
      }
    }
    q("#test").disabled = false;
  };
  const del = q("#delete");
  if (del) del.onclick = () => {
    q("#delete-area").innerHTML = `<span class="subtle">Remove this mount?</span> <button class="btn btn-danger" id="really">Delete</button> <button class="btn" id="keep">Keep</button>`;
    q("#really").onclick = async () => {
      try { await invoke("delete_mount", { name: v.original }); view = { kind: "list" }; toast("Mount removed"); await refresh(); }
      catch (err) { toast(String(err), true); }
    };
    q("#keep").onclick = () => renderEditor();
  };
  renderEditorStatus();
}

async function renderEditorStatus() {
  if (view.kind !== "edit" || view.original === null) return;
  const panel = $app.querySelector("#status-panel");
  const m = snap.mounts.find((x) => x.config.name === view.original);
  if (!panel || !m) return;
  const lines = await invoke("log_tail", { name: m.config.name });
  const log = lines.map((l) => `<span class="${/ ERROR /.test(l) ? "err" : ""}">${esc(l)}</span>`).join("\n");
  const facts = [
    m.pid ? `rclone pid ${m.pid}` : null,
    m.config.mode === "sync" && m.last_ok_secs != null ? `last synced ${m.last_ok_secs}s ago` : null,
    `${m.restarts} restart${m.restarts === 1 ? "" : "s"}`,
    m.config.mode !== "sync" && m.last_ok_secs != null ? `last healthy ${m.last_ok_secs}s ago` : null,
    m.uploads_pending ? `${m.uploads_pending} file(s) uploading` : null,
  ].filter(Boolean);
  const wasAtBottom = (() => { const p = panel.querySelector("pre"); return !p || p.scrollTop + p.clientHeight >= p.scrollHeight - 8; })();
  panel.innerHTML = `
    <div class="head"><span class="dot" style="background:${m.color}"></span><span class="state" style="color:${m.color}">${esc(m.state_label)}</span>${m.detail !== m.state_label ? `<span class="subtle">${esc(m.detail)}</span>` : ""}</div>
    <div class="facts">${facts.map((f) => `<span class="tiny">${esc(f)}</span>`).join("")}
      <button class="btn btn-sm ${m.needs_resync ? "btn-primary" : ""}" id="restart">${m.needs_resync ? "Resync" : m.config.mode === "sync" ? "Sync now" : "Restart mount"}</button><button class="btn btn-sm" id="showlog">Show log file</button>
      ${m.sso_profile ? `<button class="btn btn-sm" id="panel-signin">Sign in to AWS (${esc(m.sso_profile)})…</button>` : ""}</div>
    <pre class="log">${log || '<span class="tiny">No log output yet.</span>'}</pre>`;
  panel.querySelector("#restart").onclick = () => {
    invoke("restart_mount", { name: m.config.name });
    toast(m.needs_resync ? "Resyncing…" : m.config.mode === "sync" ? "Syncing…" : "Restarting…");
  };
  panel.querySelector("#showlog").onclick = () => invoke("show_log", { name: m.config.name });
  const ps = panel.querySelector("#panel-signin");
  if (ps) ps.onclick = () => signIn(m.config);
  const pre = panel.querySelector("pre");
  if (wasAtBottom) pre.scrollTop = pre.scrollHeight;
}

// ------------------------------------------------------------------ modal

function renderModal() {
  if (!snap.show_login_prompt) { $modal.classList.add("hidden"); return; }
  if (!$modal.classList.contains("hidden")) return;
  $modal.classList.remove("hidden");
  $modal.innerHTML = `
    <div class="dialog">
      <h3>Start BucketMount at login?</h3>
      <p>BucketMount keeps your buckets mounted and watches the connection. Starting it automatically means your volumes are available as soon as you log in. You can change this later from the mount list.</p>
      <div class="buttons"><button class="btn" id="later">Not now</button><button class="btn btn-primary" id="yes">Start at login</button></div>
    </div>`;
  const answer = async (enabled) => {
    invoke("debug_log", { msg: `login prompt answered: ${enabled ? "start at login" : "not now"}` });
    try { await invoke("set_start_at_login", { enabled }); }
    catch (err) { toast(String(err), true); }
    $modal.classList.add("hidden");
    await refresh();
  };
  $modal.querySelector("#yes").onclick = () => answer(true);
  $modal.querySelector("#later").onclick = () => answer(false);
}

// ------------------------------------------------------------------ boot

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && view.kind === "edit") { view = { kind: "list" }; render(); }
});
listen("state-changed", refresh);
listen("toast", (e) => toast(e.payload));
listen("toast-error", (e) => toast(e.payload, true));
listen("edit-mount", (e) => { if (snap) openEditor(e.payload); });
refresh();
