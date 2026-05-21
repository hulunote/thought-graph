// ThoughtGraph Web frontend.
//
// Ported from the Tauri main.js with one substantive change: window.__TAURI__
// is replaced by a fetch-based invoke() that hits POST /api/invoke. The
// argument-shape convention (camelCase) is preserved so the rest of the file
// is identical to the desktop version.

async function invoke(cmd, args) {
  const resp = await fetch("/api/invoke", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ cmd, args: args || {} }),
  });
  // Try to parse JSON regardless of status — server returns {error} on failure.
  let payload;
  try {
    payload = await resp.json();
  } catch {
    throw new Error(`HTTP ${resp.status} (non-JSON response)`);
  }
  if (!resp.ok) {
    throw new Error(payload && payload.error ? payload.error : `HTTP ${resp.status}`);
  }
  return payload;
}

window.addEventListener("error", (e) => console.error("[ui]", e.error || e.message));
window.addEventListener("unhandledrejection", (e) => console.error("[ui] promise", e.reason));

// ============================================================================
// state
// ============================================================================

const state = {
  graphs: [],
  currentGraph: null,
  nodes: [],
  edges: [],
  selectedNodeId: null,
  searchHits: null,
  searchQuery: "",
};

const LS = {
  sidebar() { return localStorage.getItem("ui.sidebar") !== "0"; },
  setSidebar(v) { localStorage.setItem("ui.sidebar", v ? "1" : "0"); },
  pathpane() { return localStorage.getItem("ui.pathpane") !== "0"; },
  setPathpane(v) { localStorage.setItem("ui.pathpane", v ? "1" : "0"); },
};

// ============================================================================
// utilities
// ============================================================================

const $ = (sel) => document.querySelector(sel);

function fmtDate(s) { try { return new Date(s).toLocaleString(); } catch { return s; } }

function escapeHtml(s) {
  return (s || "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

function highlight(text, query) {
  if (!query || !text) return escapeHtml(text);
  const terms = query.trim().split(/\s+/).filter(Boolean);
  if (!terms.length) return escapeHtml(text);
  const esc = escapeHtml(text);
  let out = esc;
  for (const t of terms) {
    const re = new RegExp("(" + t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "gi");
    out = out.replace(re, '<mark class="hl">$1</mark>');
  }
  return out;
}

function genAppId() {
  const alpha = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  const bytes = crypto.getRandomValues(new Uint8Array(10));
  let s = "";
  for (let i = 0; i < 10; i++) s += alpha[bytes[i] % alpha.length];
  return s;
}

async function createNodeAuto(graphId, content, parentNodeId) {
  let lastErr;
  for (let attempt = 0; attempt < 5; attempt++) {
    const appId = genAppId();
    try {
      return await invoke("create_node", { graphId, appId, content, parentNodeId });
    } catch (e) {
      lastErr = e;
      const msg = String(e || "");
      if (!msg.includes("already exists")) throw e;
    }
  }
  throw new Error("Failed to allocate unique app_id after 5 attempts: " + lastErr);
}

function debounce(fn, ms) {
  let t;
  return (...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}

// Open a server-side rendered/exported file in a new tab. The server returns
// {url: "/exports/..."} — the browser then downloads or renders inline based
// on Content-Type.
function openExport(url) {
  if (!url) return;
  window.open(url, "_blank", "noopener");
}

// ============================================================================
// in-browser Graphviz rendering via vendored Viz.js (WASM build of `dot`).
//
// Why: corporate boxes may forbid installing GraphViz system-wide. Viz.js is
// vendored under web-src/vendor/viz-standalone.js, runs entirely in the
// browser, and produces SVG. No `dot` binary, no network at runtime.
// ============================================================================

let _vizInstancePromise = null;
function getViz() {
  if (!_vizInstancePromise) {
    if (typeof Viz === "undefined" || !Viz.instance) {
      return Promise.reject(new Error(
        "Viz.js failed to load. Make sure web-src/vendor/viz-standalone.js exists."
      ));
    }
    _vizInstancePromise = Viz.instance();
  }
  return _vizInstancePromise;
}

// Server's wrap_label hard-breaks long tokens at exactly this many chars.
// A line at (or one shy of) this length followed by URL-safe chars is almost
// certainly a wrap-continuation, not a deliberate newline in user content.
const SERVER_WRAP_WIDTH = 16;

// Collapse `https://xlisp.gi\nthub.io/posts/co\nmpression-is-int\nelligence.html`
// (server-wrapped URL fragments) back into one line, then shorten to
// `https://host/…`. Unrelated lines (e.g. a trailing "url" annotation) are
// preserved untouched.
function compactUrlsInLabel(labelText) {
  const lines = labelText.split("\\n");
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const decoded = lines[i].replace(/\\(.)/g, "$1");
    if (/^https?:\/\//.test(decoded)) {
      let url = decoded;
      let j = i + 1;
      while (j < lines.length) {
        const prevRaw = lines[j - 1].replace(/\\(.)/g, "$1");
        const nextRaw = lines[j].replace(/\\(.)/g, "$1");
        const wasHardWrap =
          prevRaw.length >= SERVER_WRAP_WIDTH - 1 && !prevRaw.endsWith(" ");
        const looksUrlish = /^[A-Za-z0-9._~:/?#\[\]@!$&'()*+,;%=\-]+$/.test(nextRaw);
        if (wasHardWrap && looksUrlish) {
          url += nextRaw;
          j++;
        } else {
          break;
        }
      }
      out.push(shortenUrl(url));
      i = j;
    } else {
      out.push(lines[i]);
      i++;
    }
  }
  return out.join("\\n");
}

function shortenUrl(url) {
  const m = url.match(/^(https?:\/\/)([^\/?#]+)([\/?#].*)?$/);
  if (!m) return url;
  const [, proto, host, rest] = m;
  if (!rest || rest === "/" || (proto + host + rest).length <= 24) {
    return proto + host + (rest || "");
  }
  return `${proto}${host}/…`;
}

// Viz.js (WASM graphviz) has no access to the system's CJK fonts, so it
// estimates Chinese/Japanese/Korean glyphs using Latin metrics — about half
// their actual rendered width. Even ASCII underestimates here, because the
// DOT requests `fontname=STHeiti` and the browser falls back to a CJK font
// whose Latin glyphs are wider than Viz.js's internal Helvetica estimate.
// The result: node boxes come out too narrow, and the browser paints the
// real glyphs spilling out the sides. Server DOT stays unchanged (the
// desktop `dot` binary with STHeiti measures correctly); we only widen
// for the in-browser renderer.
//
// We also collapse server-wrapped URLs into a compact `proto://host/…` form
// so they don't dominate the layout.
//
// width/height attrs are interpreted as MIN size when fixedsize=false
// (graphviz's default); we size them from a CJK-aware char count.
function widenCjkLabelsForViz(dot) {
  return dot.replace(
    /^([ \t]*)(n\d+|graph_root)([ \t]*)\[([^\]]*)\]/gm,
    (match, indent, nodeId, sp, attrs) => {
      if (/\bwidth\s*=/.test(attrs)) return match;
      const labelMatch = attrs.match(/\blabel="((?:[^"\\]|\\.)*)"/);
      if (!labelMatch) return match;
      const compacted = compactUrlsInLabel(labelMatch[1]);
      const lines = compacted.split("\\n");
      let maxLineWidth = 0;
      for (const raw of lines) {
        const text = raw.replace(/\\(.)/g, "$1");
        let w = 0;
        for (const ch of text) {
          const cp = ch.codePointAt(0);
          const wide =
            (cp >= 0x2e80 && cp <= 0x9fff) ||   // CJK Radicals → Unified Ideographs
            (cp >= 0xac00 && cp <= 0xd7af) ||   // Hangul Syllables
            (cp >= 0xff00 && cp <= 0xffef) ||   // Halfwidth/Fullwidth Forms
            (cp >= 0x3000 && cp <= 0x303f);     // CJK Symbols and Punctuation
          w += wide ? 0.18 : 0.10;
        }
        if (w > maxLineWidth) maxLineWidth = w;
      }
      // Pad just enough for the rounded corner; this matches graphviz's own
      // default node margin (0.11,0.055) so boxes hug the text instead of
      // floating in whitespace.
      const widthIn = (maxLineWidth + 0.15).toFixed(2);
      const heightIn = (lines.length * 0.21 + 0.1).toFixed(2);
      const newAttrs = compacted !== labelMatch[1]
        ? attrs.replace(/\blabel="(?:[^"\\]|\\.)*"/, `label="${compacted}"`)
        : attrs;
      return `${indent}${nodeId}${sp}[${newAttrs.trim()}, width=${widthIn}, height=${heightIn}]`;
    },
  );
}

// Render a DOT source string and open it in a new tab as an SVG document.
// The new tab is suitable for browser File → Print → Save as PDF.
async function renderDotInNewTab(dot, titleHint) {
  const viz = await getViz();
  // renderString throws on malformed DOT; let it propagate so the caller can alert().
  const svg = viz.renderString(widenCjkLabelsForViz(dot), { format: "svg" });
  // Wrap in a minimal HTML doc so the SVG fills the viewport and is printable.
  const safeTitle = (titleHint || "ThoughtGraph").replace(/[<>&]/g, "");
  const doc = `<!doctype html><html><head><meta charset="utf-8"><title>${safeTitle}</title>
<style>
  html,body{margin:0;padding:0;height:100%;background:#fafafa;font-family:system-ui,sans-serif}
  .bar{position:fixed;top:0;left:0;right:0;padding:6px 10px;background:#fff;border-bottom:1px solid #ddd;display:flex;gap:8px;align-items:center;font-size:12px;z-index:10}
  .bar button,.bar a{padding:3px 10px;border:1px solid #bbb;background:#fff;border-radius:4px;cursor:pointer;text-decoration:none;color:#1f2330;font:inherit}
  .stage{padding:44px 12px 12px;min-height:calc(100% - 56px);overflow:auto}
  svg{max-width:100%;height:auto;display:block;margin:0 auto;background:#fafafa}
  @media print { .bar{display:none} .stage{padding:0} }
</style></head><body>
<div class="bar">
  <b>${safeTitle}</b>
  <button onclick="window.print()">Print / Save as PDF</button>
  <a id="dl-svg" download="${safeTitle}.svg">Download SVG</a>
</div>
<div class="stage" id="stage"></div>
<script>
  document.getElementById('stage').innerHTML = ${JSON.stringify(svg)};
  const blob = new Blob([${JSON.stringify(svg)}], {type: 'image/svg+xml'});
  document.getElementById('dl-svg').href = URL.createObjectURL(blob);
</script>
</body></html>`;
  const blob = new Blob([doc], { type: "text/html;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  window.open(url, "_blank", "noopener");
  // Free the URL after the new tab has had time to load.
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

// ============================================================================
// modal
// ============================================================================

function modal({ title, body, okLabel = "OK", onValidate }) {
  return new Promise((resolve) => {
    const m = $("#modal");
    $("#modal-title").textContent = title;
    const bodyEl = $("#modal-body");
    bodyEl.className = "modal-body";
    bodyEl.innerHTML = "";
    bodyEl.appendChild(body);
    $("#modal-ok").textContent = okLabel;
    m.classList.remove("hidden");

    const firstField = bodyEl.querySelector("textarea, input, button");
    if (firstField && firstField.tagName !== "BUTTON") firstField.focus();

    function cleanup(result) {
      m.classList.add("hidden");
      $("#modal-ok").onclick = null;
      $("#modal-cancel").onclick = null;
      bodyEl.innerHTML = "";
      resolve(result);
    }
    $("#modal-ok").onclick = () => {
      const v = onValidate ? onValidate() : true;
      if (v === false) return;
      cleanup(v);
    };
    $("#modal-cancel").onclick = () => cleanup(null);
  });
}

async function contentModal({ title, placeholder = "", initial = "", okLabel = "OK" }) {
  const div = document.createElement("div");
  div.innerHTML = `
    <label>Content
      <textarea id="cm-content" rows="6" placeholder="${escapeHtml(placeholder)}">${escapeHtml(initial)}</textarea>
    </label>
    <small class="muted">app_id is generated automatically.</small>
  `;
  return modal({
    title,
    body: div,
    okLabel,
    onValidate() {
      const v = div.querySelector("#cm-content").value;
      if (!v.trim()) return false;
      return { content: v };
    },
  });
}

async function refTargetModal(fromNode) {
  const candidates = state.nodes.filter((n) => n.id !== fromNode.id);
  if (!candidates.length) { alert("This graph has no other nodes to reference yet."); return null; }
  const div = document.createElement("div");
  div.innerHTML = `
    <p class="muted">Pick the target node — adds a <b>ref</b> edge from <code>${escapeHtml(fromNode.app_id)}</code> to it. This may close a cycle.</p>
    <input id="cm-filter" type="search" placeholder="Filter…" autofocus />
    <ul id="cm-list" class="picker-list"></ul>
    <label>Edge label (optional)
      <input id="cm-label" placeholder="e.g. depends-on, contradicts" />
    </label>
  `;
  let picked = null;
  function render(filter = "") {
    const f = filter.toLowerCase();
    const ul = div.querySelector("#cm-list");
    const items = candidates.filter(
      (n) => n.app_id.toLowerCase().includes(f) || n.content.toLowerCase().includes(f),
    );
    ul.innerHTML = items.map((n) =>
      `<li data-id="${n.id}">
         <span class="app-id">${escapeHtml(n.app_id)}</span>
         <span>${escapeHtml(n.content.length > 120 ? n.content.slice(0, 120) + "…" : n.content)}</span>
       </li>`).join("");
    for (const li of ul.querySelectorAll("li")) {
      li.onclick = () => {
        picked = Number(li.dataset.id);
        ul.querySelectorAll("li").forEach((x) => x.classList.remove("picked"));
        li.classList.add("picked");
      };
    }
  }
  render();
  div.querySelector("#cm-filter").oninput = (e) => render(e.target.value);
  return modal({
    title: "Reference an existing node (⟲)",
    body: div,
    okLabel: "Add reference",
    onValidate() {
      if (!picked) { alert("Select a target node first."); return false; }
      const target = candidates.find((n) => n.id === picked);
      const label = div.querySelector("#cm-label").value || "";
      return { target, label };
    },
  });
}

// ============================================================================
// graphs
// ============================================================================

async function loadGraphs() {
  state.graphs = await invoke("list_graphs");
  renderGraphList();
}

function renderGraphList() {
  const ul = $("#graph-list");
  ul.innerHTML = "";
  for (const g of state.graphs) {
    const li = document.createElement("li");
    if (state.currentGraph?.id === g.id) li.classList.add("active");
    li.innerHTML = `<div class="gname">${escapeHtml(g.name)}</div><div class="gdate">${fmtDate(g.updated_at)}</div>`;
    li.onclick = () => selectGraph(g.id);
    ul.appendChild(li);
  }
}

async function selectGraph(id) {
  state.currentGraph = state.graphs.find((g) => g.id === id) || null;
  state.selectedNodeId = null;
  state.searchHits = null;
  state.searchQuery = "";
  $("#content-search").value = "";
  $("#search-count").textContent = "";
  $("#from-input").value = "";
  $("#to-input").value = "";
  $("#path-results").innerHTML = "";
  renderGraphList();
  if (!state.currentGraph) return;
  $("#graph-title").textContent = state.currentGraph.name;
  $("#graph-desc").textContent = state.currentGraph.description || "";
  await reloadNodesAndEdges();
}

async function reloadNodesAndEdges() {
  if (!state.currentGraph) return;
  const [nodes, edges] = await Promise.all([
    invoke("list_nodes", { graphId: state.currentGraph.id }),
    invoke("list_edges", { graphId: state.currentGraph.id }),
  ]);
  state.nodes = nodes;
  state.edges = edges;
  renderTree();
}

// ============================================================================
// tree
// ============================================================================

function buildTreeIndex() {
  const replyParent = new Map();
  const outgoingByFrom = new Map();
  for (const e of state.edges) {
    if (e.kind === "reply") replyParent.set(e.to_node_id, e.from_node_id);
    if (!outgoingByFrom.has(e.from_node_id)) outgoingByFrom.set(e.from_node_id, []);
    outgoingByFrom.get(e.from_node_id).push(e);
  }
  const children = new Map();
  for (const n of state.nodes) {
    const p = replyParent.get(n.id);
    if (p) (children.get(p) || children.set(p, []).get(p)).push(n);
  }
  const roots = state.nodes.filter((n) => !replyParent.has(n.id));
  return { roots, children, outgoingByFrom };
}

function renderTree() {
  const ul = $("#nodes-tree");
  ul.innerHTML = "";
  if (state.nodes.length === 0) {
    ul.innerHTML = `<li class="muted" style="padding:10px">No nodes yet. Click <b>+ New comment</b> to start.</li>`;
    return;
  }

  const { roots, children, outgoingByFrom } = buildTreeIndex();
  const matchIds = new Set(state.searchHits ? state.searchHits.map((h) => h.node.id) : []);
  const nodeById = new Map(state.nodes.map((n) => [n.id, n]));

  function nodeLi(n) {
    const li = document.createElement("li");
    const card = document.createElement("div");
    card.className = "node-card";
    if (state.selectedNodeId === n.id) card.classList.add("selected");
    if (matchIds.has(n.id)) card.classList.add("match");

    const contentHtml = state.searchQuery
      ? highlight(n.content, state.searchQuery)
      : escapeHtml(n.content);

    const outgoing = outgoingByFrom.get(n.id) || [];
    const edgesHtml = outgoing.length === 0 ? "" : `
      <div class="outgoing-edges">
        ${outgoing.map((e) => {
          const target = nodeById.get(e.to_node_id);
          const tlabel = target ? target.app_id : "?";
          return `<span class="edge-pill ${e.kind}">
                    ${e.kind === "ref" ? "⟲" : "↳"} ${escapeHtml(tlabel)}${e.label ? " [" + escapeHtml(e.label) + "]" : ""}
                    <button data-edge="${e.id}" title="Delete edge">×</button>
                  </span>`;
        }).join("")}
      </div>`;

    card.innerHTML = `
      <div class="node-row">
        <span class="app-id">${escapeHtml(n.app_id)}</span>
        <div class="content-preview collapsed" data-node="${n.id}">${contentHtml}</div>
        <div class="row-actions">
          <button class="icon-btn act-reply" data-node="${n.id}" title="Reply (↳)">↳</button>
          <button class="icon-btn act-ref"   data-node="${n.id}" title="Reference (⟲) — adds a cycle">⟲</button>
          <button class="icon-btn act-edit"  data-node="${n.id}" title="Edit content">✎</button>
          <button class="icon-btn act-del danger" data-node="${n.id}" title="Delete node">✕</button>
        </div>
      </div>
      ${edgesHtml}
    `;

    card.querySelector(".act-reply").onclick = (e) => { e.stopPropagation(); doReply(n); };
    card.querySelector(".act-ref").onclick   = (e) => { e.stopPropagation(); doAddRef(n); };
    card.querySelector(".act-edit").onclick  = (e) => { e.stopPropagation(); doEdit(n); };
    card.querySelector(".act-del").onclick   = (e) => { e.stopPropagation(); doDelete(n); };
    for (const btn of card.querySelectorAll("button[data-edge]")) {
      btn.onclick = async (ev) => {
        ev.stopPropagation();
        await invoke("delete_edge", { edgeId: Number(btn.dataset.edge) });
        await reloadNodesAndEdges();
      };
    }

    const cp = card.querySelector(".content-preview");
    cp.onclick = () => {
      state.selectedNodeId = n.id;
      cp.classList.toggle("collapsed");
      renderTree();
    };

    li.appendChild(card);
    const kids = children.get(n.id) || [];
    if (kids.length) {
      const sub = document.createElement("ul");
      for (const k of kids) sub.appendChild(nodeLi(k));
      li.appendChild(sub);
    }
    return li;
  }

  for (const r of roots) ul.appendChild(nodeLi(r));
}

function scrollToNode(nodeId) {
  state.selectedNodeId = nodeId;
  renderTree();
  setTimeout(() => {
    const el = document.querySelector(`#nodes-tree .content-preview[data-node="${nodeId}"]`);
    if (el) {
      el.classList.remove("collapsed");
      el.closest(".node-card")?.scrollIntoView({ behavior: "smooth", block: "center" });
    }
  }, 0);
}

// ============================================================================
// per-node actions
// ============================================================================

async function doReply(node) {
  const r = await contentModal({
    title: `Reply to ${node.app_id}`,
    placeholder: "Your reply…",
    okLabel: "Reply",
  });
  if (!r) return;
  try {
    const created = await createNodeAuto(state.currentGraph.id, r.content, node.id);
    state.selectedNodeId = created.id;
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
}

async function doAddRef(node) {
  const r = await refTargetModal(node);
  if (!r) return;
  try {
    await invoke("add_ref_edge", {
      graphId: state.currentGraph.id,
      fromNodeId: node.id,
      toAppId: r.target.app_id,
      label: r.label,
    });
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
}

async function doEdit(node) {
  const r = await contentModal({
    title: `Edit ${node.app_id}`,
    initial: node.content,
    okLabel: "Save",
  });
  if (!r) return;
  try {
    await invoke("update_node", { nodeId: node.id, content: r.content });
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
}

async function doDelete(node) {
  if (!confirm(`Delete node "${node.app_id}"? Replies under it will also be deleted.`)) return;
  try {
    await invoke("delete_node", { nodeId: node.id });
    if (state.selectedNodeId === node.id) state.selectedNodeId = null;
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
}

// ============================================================================
// top-level actions
// ============================================================================

$("#new-graph-btn").onclick = async () => {
  const div = document.createElement("div");
  div.innerHTML = `
    <label>Name <input id="cm-name" placeholder="e.g. Q2 strategy" /></label>
    <label>Description <input id="cm-desc" placeholder="Optional" /></label>
  `;
  const r = await modal({
    title: "New graph",
    body: div,
    okLabel: "Create",
    onValidate() {
      const name = div.querySelector("#cm-name").value.trim();
      if (!name) return false;
      return { name, desc: div.querySelector("#cm-desc").value };
    },
  });
  if (!r) return;
  const g = await invoke("create_graph", { name: r.name, description: r.desc });
  await loadGraphs();
  await selectGraph(g.id);
};

$("#rename-graph").onclick = async () => {
  if (!state.currentGraph) return;
  const div = document.createElement("div");
  div.innerHTML = `
    <label>Name <input id="cm-name" value="${escapeHtml(state.currentGraph.name)}" /></label>
    <label>Description <input id="cm-desc" value="${escapeHtml(state.currentGraph.description)}" /></label>
  `;
  const r = await modal({
    title: "Rename graph",
    body: div,
    okLabel: "Save",
    onValidate() {
      const name = div.querySelector("#cm-name").value.trim();
      if (!name) return false;
      return { name, desc: div.querySelector("#cm-desc").value };
    },
  });
  if (!r) return;
  await invoke("rename_graph", { id: state.currentGraph.id, name: r.name, description: r.desc });
  await loadGraphs();
  await selectGraph(state.currentGraph.id);
};

$("#delete-graph").onclick = async () => {
  if (!state.currentGraph) return;
  if (!confirm(`Delete graph "${state.currentGraph.name}"? This removes all nodes and edges.`)) return;
  await invoke("delete_graph", { id: state.currentGraph.id });
  state.currentGraph = null;
  $("#graph-title").textContent = "Pick or create a graph";
  $("#graph-desc").textContent = "";
  $("#nodes-tree").innerHTML = "";
  $("#path-results").innerHTML = "";
  await loadGraphs();
};

$("#new-root").onclick = async () => {
  if (!state.currentGraph) { alert("Pick or create a graph first."); return; }
  const r = await contentModal({
    title: "New top-level comment",
    placeholder: "Your thought…",
    okLabel: "Create",
  });
  if (!r) return;
  try {
    const created = await createNodeAuto(state.currentGraph.id, r.content, null);
    state.selectedNodeId = created.id;
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
};

// ============================================================================
// content search
// ============================================================================

const runSearch = debounce(async () => {
  if (!state.currentGraph) return;
  const q = $("#content-search").value.trim();
  state.searchQuery = q;
  if (!q) {
    state.searchHits = null;
    $("#search-count").textContent = "";
    renderTree();
    return;
  }
  try {
    const hits = await invoke("search_nodes", {
      graphId: state.currentGraph.id,
      query: q,
      limit: 100,
    });
    state.searchHits = hits;
    $("#search-count").textContent = `${hits.length} match${hits.length === 1 ? "" : "es"}`;
    renderTree();
    if (hits.length) scrollToNode(hits[0].node.id);
  } catch (e) {
    state.searchHits = [];
    $("#search-count").textContent = `error: ${e}`;
    renderTree();
  }
}, 150);

$("#content-search").oninput = runSearch;

// ============================================================================
// export / render — web edition opens the resulting URL in a new tab
// ============================================================================

$("#export-gv").onclick = async () => {
  if (!state.currentGraph) return;
  try {
    const r = await invoke("export_gv", { graphId: state.currentGraph.id });
    openExport(r.url);
  } catch (e) { alert(e); }
};

// Render the current graph entirely in the browser via vendored Viz.js. The
// server only returns the DOT source string — no `dot` binary needed.
$("#render-svg").onclick = async () => {
  if (!state.currentGraph) return;
  const btn = $("#render-svg");
  const orig = btn.textContent; btn.disabled = true; btn.textContent = "Rendering…";
  try {
    const dot = await invoke("preview_dot", { graphId: state.currentGraph.id });
    await renderDotInNewTab(dot, state.currentGraph.name);
  } catch (e) { alert(e); }
  finally { btn.disabled = false; btn.textContent = orig; }
};

// ============================================================================
// pane toggles
// ============================================================================

function applyPaneState() {
  $("#app").classList.toggle("no-sidebar", !LS.sidebar());
  document.querySelector(".panes").classList.toggle("no-search", !LS.pathpane());
  $("#toggle-sidebar").textContent = LS.sidebar() ? "◀" : "▶";
  $("#toggle-pathpane").textContent = LS.pathpane() ? "▶" : "◀";
}
$("#toggle-sidebar").onclick = () => { LS.setSidebar(!LS.sidebar()); applyPaneState(); };
$("#toggle-pathpane").onclick = () => { LS.setPathpane(!LS.pathpane()); applyPaneState(); };

// ============================================================================
// path search
// ============================================================================

const MAX_PATHS = 50;

$("#search-paths").onclick = async () => {
  if (!state.currentGraph) { alert("Pick or create a graph first."); return; }
  const fromKw = $("#from-input").value.trim();
  const toKw = $("#to-input").value.trim();
  if (!fromKw) { alert("Enter a From keyword."); return; }
  if (!toKw)   { alert("Enter a To keyword."); return; }
  try {
    const hits = await invoke("find_paths_by_keyword", {
      graphId: state.currentGraph.id,
      fromKeyword: fromKw,
      toKeyword: toKw,
      maxPaths: MAX_PATHS,
    });
    renderPathResults({ fromKw, toKw, hits });
  } catch (e) {
    $("#path-results").innerHTML = `<p class="muted" style="padding:8px">${escapeHtml(String(e))}</p>`;
  }
};

function renderPathResults({ fromKw, toKw, hits }) {
  const box = $("#path-results");
  const header = `<div class="resolved-note" style="display:flex; align-items:center; justify-content:space-between; gap:8px">
       <span><code>${escapeHtml(fromKw)}</code> → <code>${escapeHtml(toKw)}</code> · ${hits.length} path${hits.length === 1 ? "" : "s"}</span>
       ${hits.length ? `<button id="open-paths-gv" class="primary" style="padding:4px 8px; font-size:11px">📈 Render paths</button>` : ""}
     </div>`;
  if (!hits.length) {
    box.innerHTML = header + `<p class="muted" style="padding:8px">No path found.</p>`;
    return;
  }
  box.innerHTML = header + hits.map((h, i) => {
    const parts = [];
    for (let j = 0; j < h.nodes.length; j++) {
      if (j > 0) {
        const step = h.steps[j - 1];
        const glyph = step.kind === "ref" ? "⟲" : "↳";
        const cls = step.kind === "ref" ? "ref" : "reply";
        parts.push(`<span class="arrow ${cls}" title="${step.kind}${step.label ? " · " + escapeHtml(step.label) : ""}"> ${glyph} </span>`);
      }
      const n = h.nodes[j];
      const preview = (n.content || "").replace(/\s+/g, " ").slice(0, 60);
      parts.push(`<span class="step" data-node="${n.id}" title="${escapeHtml(n.content.slice(0, 200))}"><span class="app-id">${escapeHtml(n.app_id)}</span> <span class="step-preview">${escapeHtml(preview)}</span></span>`);
    }
    return `<div class="path-item"><b>Path ${i + 1}</b> · ${h.nodes.length - 1} step(s)<br/>${parts.join("")}</div>`;
  }).join("");

  for (const s of box.querySelectorAll(".step[data-node]")) {
    s.style.cursor = "pointer";
    s.onclick = () => scrollToNode(Number(s.dataset.node));
  }

  const openBtn = box.querySelector("#open-paths-gv");
  if (openBtn) {
    openBtn.onclick = async () => {
      openBtn.disabled = true;
      const origText = openBtn.textContent;
      openBtn.textContent = "Rendering…";
      try {
        const dot = await invoke("preview_paths_dot_by_keyword", {
          graphId: state.currentGraph.id,
          fromKeyword: fromKw,
          toKeyword: toKw,
          maxPaths: MAX_PATHS,
        });
        await renderDotInNewTab(dot, `${state.currentGraph.name} — paths`);
      } catch (e) { alert(e); }
      finally { openBtn.disabled = false; openBtn.textContent = origText; }
    };
  }
}

for (const id of ["#from-input", "#to-input"]) {
  $(id).addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); $("#search-paths").click(); }
  });
}

// ============================================================================
// boot
// ============================================================================

applyPaneState();
loadGraphs().catch((e) => {
  document.body.innerHTML =
    '<pre style="padding:20px;color:#b32424;background:#fff;font-family:monospace">' +
    'Failed to reach the ThoughtGraph server.\n\n' + escapeHtml(String(e)) +
    '\n\nMake sure `thoughtgraph-web` is running and you opened the URL it printed.' +
    '</pre>';
});
