const invoke = window.__TAURI__.core.invoke;

const state = {
  graphs: [],
  currentGraph: null,
  nodes: [],
  edges: [],
  selectedNodeId: null,
};

// ---------- helpers ----------
const $ = (sel) => document.querySelector(sel);

function fmtDate(s) {
  try { return new Date(s).toLocaleString(); } catch { return s; }
}

function preview(text, n = 60) {
  const one = (text || "").replace(/\s+/g, " ");
  return one.length > n ? one.slice(0, n) + "…" : one;
}

function escapeHtml(s) {
  return (s || "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

// ---------- modal ----------
function modal({ title, fields, okLabel = "OK" }) {
  return new Promise((resolve) => {
    const m = $("#modal");
    $("#modal-title").textContent = title;
    const body = $("#modal-body");
    body.className = "modal-body";
    body.innerHTML = fields.map((f, i) => {
      const id = `mf_${i}`;
      const value = escapeHtml(f.value || "");
      if (f.type === "textarea") {
        return `<label>${escapeHtml(f.label)}<textarea id="${id}" rows="${f.rows || 4}">${value}</textarea></label>`;
      }
      return `<label>${escapeHtml(f.label)}<input id="${id}" type="text" placeholder="${escapeHtml(f.placeholder || "")}" value="${value}" /></label>`;
    }).join("");
    $("#modal-ok").textContent = okLabel;
    m.classList.remove("hidden");
    const firstInput = body.querySelector("input,textarea");
    if (firstInput) firstInput.focus();

    function cleanup(result) {
      m.classList.add("hidden");
      $("#modal-ok").onclick = null;
      $("#modal-cancel").onclick = null;
      resolve(result);
    }
    $("#modal-ok").onclick = () => {
      const values = fields.map((f, i) => body.querySelector(`#mf_${i}`).value);
      cleanup(values);
    };
    $("#modal-cancel").onclick = () => cleanup(null);
  });
}

// ---------- graphs ----------
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
  renderDetail();
  refreshDotPreview();
}

// ---------- tree ----------
function renderTree() {
  const ul = $("#nodes-tree");
  ul.innerHTML = "";
  if (state.nodes.length === 0) {
    ul.innerHTML = `<li class="muted" style="padding:8px">No nodes yet. Click <b>+ New comment</b> to start.</li>`;
    return;
  }
  // Build parent->children using reply edges. Each node has at most one reply parent.
  const replyParents = new Map();
  for (const e of state.edges) {
    if (e.kind === "reply") replyParents.set(e.to_node_id, e.from_node_id);
  }
  const children = new Map();
  for (const n of state.nodes) {
    const p = replyParents.get(n.id);
    if (p) {
      if (!children.has(p)) children.set(p, []);
      children.get(p).push(n);
    }
  }
  const roots = state.nodes.filter((n) => !replyParents.has(n.id));

  function nodeLi(n) {
    const li = document.createElement("li");
    const row = document.createElement("div");
    row.className = "row" + (state.selectedNodeId === n.id ? " selected" : "");
    row.innerHTML = `<span class="app-id">${escapeHtml(n.app_id)}</span><span class="content-preview">${escapeHtml(preview(n.content, 80))}</span>`;
    row.onclick = () => {
      state.selectedNodeId = n.id;
      renderTree();
      renderDetail();
    };
    li.appendChild(row);
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

// ---------- detail ----------
function selectedNode() {
  return state.nodes.find((n) => n.id === state.selectedNodeId) || null;
}

function renderDetail() {
  const node = selectedNode();
  const body = $("#detail-body");
  const has = !!node;
  $("#add-reply").disabled = !has;
  $("#add-ref").disabled = !has;
  $("#delete-node").disabled = !has;
  if (!has) {
    $("#detail-title").textContent = "Detail";
    body.innerHTML = `<p class="muted">Select a node to view and edit its content.</p>`;
    $("#edges-list").innerHTML = "";
    return;
  }
  $("#detail-title").textContent = node.app_id;
  body.innerHTML = `
    <label>app_id <input id="d-app-id" value="${escapeHtml(node.app_id)}" disabled /></label>
    <label>Content
      <textarea id="d-content" rows="8">${escapeHtml(node.content)}</textarea>
    </label>
    <div style="display:flex; gap:8px; margin-top:8px">
      <button id="save-node" class="primary">Save</button>
      <span class="meta">created ${fmtDate(node.created_at)}</span>
    </div>
  `;
  $("#save-node").onclick = async () => {
    const newContent = $("#d-content").value;
    await invoke("update_node", { nodeId: node.id, content: newContent });
    await reloadNodesAndEdges();
  };

  // edges list (outgoing)
  const out = state.edges.filter((e) => e.from_node_id === node.id);
  const nodeById = new Map(state.nodes.map((n) => [n.id, n]));
  const list = $("#edges-list");
  list.innerHTML = "";
  if (out.length === 0) {
    list.innerHTML = `<li class="muted">No outgoing edges.</li>`;
  } else {
    for (const e of out) {
      const target = nodeById.get(e.to_node_id);
      const li = document.createElement("li");
      li.innerHTML = `
        <span>
          <span class="kind-${e.kind}">${e.kind === "ref" ? "⟲ ref" : "↳ reply"}</span>
          → <span class="app-id">${escapeHtml(target?.app_id || "?")}</span>
          ${e.label ? ` <span class="muted">[${escapeHtml(e.label)}]</span>` : ""}
        </span>
        <button data-id="${e.id}" class="del-edge">×</button>
      `;
      list.appendChild(li);
    }
    for (const btn of list.querySelectorAll(".del-edge")) {
      btn.onclick = async () => {
        await invoke("delete_edge", { edgeId: Number(btn.dataset.id) });
        await reloadNodesAndEdges();
      };
    }
  }
}

// ---------- actions ----------
$("#new-graph-btn").onclick = async () => {
  const r = await modal({
    title: "New graph",
    fields: [
      { label: "Name", placeholder: "e.g. Q2 strategy" },
      { label: "Description", placeholder: "Optional" },
    ],
    okLabel: "Create",
  });
  if (!r) return;
  const [name, desc] = r;
  if (!name.trim()) return;
  const g = await invoke("create_graph", { name: name.trim(), description: desc });
  await loadGraphs();
  await selectGraph(g.id);
};

$("#rename-graph").onclick = async () => {
  if (!state.currentGraph) return;
  const r = await modal({
    title: "Rename graph",
    fields: [
      { label: "Name", value: state.currentGraph.name },
      { label: "Description", value: state.currentGraph.description },
    ],
    okLabel: "Save",
  });
  if (!r) return;
  const [name, desc] = r;
  await invoke("rename_graph", { id: state.currentGraph.id, name, description: desc });
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
  $("#edges-list").innerHTML = "";
  $("#detail-body").innerHTML = `<p class="muted">Select a node to view and edit its content.</p>`;
  $("#dot-preview").textContent = "";
  await loadGraphs();
};

$("#new-root").onclick = async () => {
  if (!state.currentGraph) { alert("Pick or create a graph first."); return; }
  const r = await modal({
    title: "New top-level comment",
    fields: [
      { label: "app_id (unique within graph)", placeholder: "e.g. root, idea-1" },
      { label: "Content", type: "textarea", rows: 6 },
    ],
    okLabel: "Create",
  });
  if (!r) return;
  const [app_id, content] = r;
  if (!app_id.trim()) return;
  try {
    await invoke("create_node", {
      graphId: state.currentGraph.id,
      appId: app_id.trim(),
      content,
      parentNodeId: null,
    });
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
};

$("#add-reply").onclick = async () => {
  const node = selectedNode();
  if (!node) return;
  const r = await modal({
    title: `Reply to ${node.app_id}`,
    fields: [
      { label: "app_id (unique within graph)", placeholder: "e.g. counter-1" },
      { label: "Content", type: "textarea", rows: 6 },
    ],
    okLabel: "Reply",
  });
  if (!r) return;
  const [app_id, content] = r;
  if (!app_id.trim()) return;
  try {
    const created = await invoke("create_node", {
      graphId: state.currentGraph.id,
      appId: app_id.trim(),
      content,
      parentNodeId: node.id,
    });
    state.selectedNodeId = created.id;
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
};

$("#add-ref").onclick = async () => {
  const node = selectedNode();
  if (!node) return;
  const existing = state.nodes.map((n) => n.app_id).join(", ");
  const r = await modal({
    title: `Reference an existing app_id from ${node.app_id}`,
    fields: [
      { label: `Target app_id (existing in this graph). Available: ${existing}`, placeholder: "e.g. root" },
      { label: "Edge label (optional)", placeholder: "e.g. depends-on, contradicts" },
    ],
    okLabel: "Add reference",
  });
  if (!r) return;
  const [target, label] = r;
  if (!target.trim()) return;
  try {
    await invoke("add_ref_edge", {
      graphId: state.currentGraph.id,
      fromNodeId: node.id,
      toAppId: target.trim(),
      label: label || "",
    });
    await reloadNodesAndEdges();
  } catch (e) { alert(e); }
};

$("#delete-node").onclick = async () => {
  const node = selectedNode();
  if (!node) return;
  if (!confirm(`Delete node "${node.app_id}"? Replies under it will also be deleted.`)) return;
  await invoke("delete_node", { nodeId: node.id });
  state.selectedNodeId = null;
  await reloadNodesAndEdges();
};

$("#export-gv").onclick = async () => {
  if (!state.currentGraph) return;
  try {
    const path = await invoke("export_gv", { graphId: state.currentGraph.id });
    alert(`Exported:\n${path}`);
  } catch (e) { alert(e); }
};

$("#render-pdf").onclick = async () => {
  if (!state.currentGraph) return;
  try {
    const res = await invoke("render_and_open", { graphId: state.currentGraph.id, format: "pdf" });
    console.log(res);
  } catch (e) { alert(e); }
};

$("#open-graphviz").onclick = async () => {
  if (!state.currentGraph) return;
  try {
    const path = await invoke("open_in_graphviz_app", { graphId: state.currentGraph.id });
    console.log("opened:", path);
  } catch (e) { alert(e); }
};

$("#show-dot").onclick = () => refreshDotPreview(true);

async function refreshDotPreview(force = false) {
  if (!state.currentGraph) { $("#dot-preview").textContent = ""; return; }
  try {
    const dot = await invoke("preview_dot", { graphId: state.currentGraph.id });
    $("#dot-preview").textContent = dot;
  } catch (e) {
    if (force) alert(e);
  }
}

$("#search-paths").onclick = async () => {
  if (!state.currentGraph) return;
  const from = $("#from-id").value.trim();
  const to = $("#to-id").value.trim();
  if (!from || !to) { alert("Enter both from and to app_ids"); return; }
  try {
    const hits = await invoke("find_paths", {
      graphId: state.currentGraph.id,
      fromAppId: from,
      toAppId: to,
      maxPaths: 10,
    });
    renderPathResults(hits);
  } catch (e) { alert(e); }
};

function renderPathResults(hits) {
  const box = $("#path-results");
  if (!hits.length) {
    box.innerHTML = `<p class="muted">No path found.</p>`;
    return;
  }
  box.innerHTML = hits.map((h, i) => {
    const steps = h.nodes.map((n, idx) => {
      const kind = idx > 0 ? h.edge_kinds[idx - 1] : null;
      const arrow = kind ? `<span class="arrow ${kind === "ref" ? "ref" : ""}">${kind === "ref" ? " ⟲ " : " ↳ "}</span>` : "";
      return `${arrow}<span class="step"><span class="app-id">${escapeHtml(n.app_id)}</span></span>`;
    }).join("");
    return `<div class="path-item"><b>Path ${i + 1}</b> · ${h.nodes.length - 1} step(s)<br/>${steps}</div>`;
  }).join("");
}

// boot
loadGraphs();
