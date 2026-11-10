// ReelVault browser client. Plain JS, fetch-only, no dependencies.
(function () {
  "use strict";

  var state = {
    q: "",
    collection: null,
    collectionName: null,
    page: 1,
    pageSize: 50,
    total: 0,
    reel: null,
  };

  function $(id) { return document.getElementById(id); }

  function setStatus() {
    fetch("/api/status")
      .then(function (r) { return r.json(); })
      .then(function (s) {
        var ai = s.ollama_available
          ? '<span class="on">Ollama on (' + s.ollama_models.length + ' models)</span>'
          : '<span class="off">Ollama not detected</span>';
        $("status-line").innerHTML =
          "v" + s.version + " | " + ai +
          " | " + s.reels + " reels, " + s.collections + " collections, " + s.notes + " notes";
        $("ai-toggle").checked = s.ai_opt_in;
        if (!$("ai-model").value) { $("ai-model").value = s.ai_model; }
      })
      .catch(function () {
        $("status-line").textContent = "status unavailable";
      });
  }

  function loadCollections() {
    fetch("/api/collections")
      .then(function (r) { return r.json(); })
      .then(function (list) {
        var ul = $("collections");
        ul.innerHTML = "";
        list.forEach(function (c) {
          var li = document.createElement("li");
          var b = document.createElement("button");
          b.type = "button";
          b.textContent = c.name + " (" + c.reels + ")";
          b.className = c.id === state.collection ? "active" : "";
          b.setAttribute("aria-label", "Filter by collection " + c.name);
          b.addEventListener("click", function () {
            if (state.collection === c.id) {
              state.collection = null;
              state.collectionName = null;
            } else {
              state.collection = c.id;
              state.collectionName = c.name;
            }
            state.page = 1;
            loadEntries();
            loadCollections();
          });
          li.appendChild(b);
          ul.appendChild(li);
        });
      });
  }

  function loadEntries() {
    var params = new URLSearchParams();
    if (state.q) { params.set("q", state.q); }
    if (state.collection) { params.set("collection", state.collection); }
    params.set("page", state.page);
    params.set("page_size", state.pageSize);
    fetch("/api/entries?" + params.toString())
      .then(function (r) { return r.json(); })
      .then(function (data) {
        state.total = data.total;
        var body = $("entries-body");
        body.innerHTML = "";
        if (!data.items.length) {
          var tr = document.createElement("tr");
          var td = document.createElement("td");
          td.colSpan = 4;
          td.className = "empty";
          td.textContent = "No reels yet. Import an Instagram export to start.";
          tr.appendChild(td);
          body.appendChild(tr);
        }
        data.items.forEach(function (row) {
          var tr = document.createElement("tr");

          var linkTd = document.createElement("td");
          var link = document.createElement("a");
          link.href = row.uri;
          link.target = "_blank";
          link.rel = "noopener noreferrer";
          link.textContent = row.code;
          linkTd.appendChild(link);
          tr.appendChild(linkTd);

          var titleTd = document.createElement("td");
          titleTd.className = "title";
          titleTd.textContent = row.title || row.code;
          titleTd.setAttribute("role", "button");
          titleTd.setAttribute("tabindex", "0");
          titleTd.setAttribute("aria-label", "Open details for " + row.code);
          titleTd.addEventListener("click", function () { openReel(row.id); });
          titleTd.addEventListener("keydown", function (e) {
            if (e.key === "Enter" || e.key === " ") { e.preventDefault(); openReel(row.id); }
          });
          tr.appendChild(titleTd);

          var notesTd = document.createElement("td");
          notesTd.textContent = row.note_count;
          tr.appendChild(notesTd);

          var collTd = document.createElement("td");
          (row.collections || []).forEach(function (name) {
            var span = document.createElement("span");
            span.className = "tag";
            span.textContent = name;
            collTd.appendChild(span);
          });
          tr.appendChild(collTd);
          body.appendChild(tr);
        });
        var pages = Math.max(1, Math.ceil(data.total / data.page_size));
        $("results-meta").textContent = state.total + " reels";
        $("collection-filter").textContent = state.collectionName
          ? "in " + state.collectionName : "";
        $("page-info").textContent = "page " + data.page + " of " + pages;
        $("prev").disabled = data.page <= 1;
        $("next").disabled = data.page >= pages;
      })
      .catch(function (e) { showReport("import-report", "Search failed: " + e, false); });
  }

  function openReel(id) {
    fetch("/api/reel/" + id)
      .then(function (r) { return r.json(); })
      .then(function (d) {
        state.reel = d;
        $("drawer-title").textContent = d.title || d.code;
        $("drawer-link").href = d.uri;
        $("drawer-collections").textContent =
          d.collections.length ? "Collections: " + d.collections.join(", ") : "No collections";
        renderNotes(d.notes);
        $("note-body").value = "";
        $("drawer").hidden = false;
        $("drawer-close").focus();
      });
  }

  function renderNotes(notes) {
    var ul = $("notes");
    ul.innerHTML = "";
    if (!notes.length) {
      var li = document.createElement("li");
      li.className = "empty";
      li.textContent = "No notes yet.";
      ul.appendChild(li);
      return;
    }
    notes.forEach(function (n) {
      var li = document.createElement("li");
      var del = document.createElement("button");
      del.type = "button";
      del.className = "del";
      del.textContent = "x";
      del.setAttribute("aria-label", "Delete note " + n.id);
      del.addEventListener("click", function () {
        if (!confirm("Delete this note?")) { return; }
        fetch("/api/notes/" + n.id, { method: "DELETE" })
          .then(function () { openReel(n.reel_id); loadEntries(); setStatus(); });
      });
      var pre = document.createElement("div");
      pre.textContent = n.body;
      var when = document.createElement("span");
      when.className = "muted";
      when.style.fontSize = "11px";
      when.textContent = new Date(n.created_at * 1000).toLocaleString();
      li.appendChild(del);
      li.appendChild(pre);
      li.appendChild(when);
      ul.appendChild(li);
    });
  }

  function addNote() {
    if (!state.reel) { return; }
    var body = $("note-body").value.trim();
    if (!body) { return; }
    fetch("/api/notes/" + state.reel.id, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ body: body }),
    })
      .then(function (r) { return r.json(); })
      .then(function (d) {
        if (d.error) { alert(d.error); return; }
        $("note-body").value = "";
        openReel(state.reel.id);
        loadEntries();
        setStatus();
      });
  }

  function showReport(id, text, ok) {
    var el = $(id);
    el.hidden = false;
    el.textContent = text;
    el.className = "report " + (ok ? "ok" : "bad");
  }

  function postJson(url, body, reportId, done) {
    fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    })
      .then(function (r) { return r.json().then(function (j) { return { status: r.status, json: j }; }); })
      .then(function (res) {
        var ok = res.status === 200;
        showReport(reportId, JSON.stringify(res.json, null, 2), ok);
        if (ok && done) { done(res.json); }
      })
      .catch(function (e) { showReport(reportId, "Request failed: " + e, false); });
  }

  function runImport() {
    var path = $("import-path").value.trim();
    if (!path) { return; }
    postJson("/api/import", { path: path }, "import-report", function () {
      loadEntries();
      loadCollections();
      setStatus();
    });
  }

  function loadSettings() {
    fetch("/api/settings")
      .then(function (r) { return r.json(); })
      .then(function (s) {
        $("ai-toggle").checked = s.ai_opt_in;
        $("ai-model").value = s.ai_model;
        $("ai-status").textContent = s.ai_opt_in
          ? "Local AI enabled. Prompts go to your machine only."
          : "Local AI off.";
      });
  }

  function init() {
    $("search-form").addEventListener("submit", function (e) {
      e.preventDefault();
      state.q = $("q").value.trim();
      state.page = 1;
      loadEntries();
    });
    $("clear-search").addEventListener("click", function () {
      $("q").value = "";
      state.q = "";
      state.page = 1;
      loadEntries();
    });
    $("prev").addEventListener("click", function () {
      if (state.page > 1) { state.page -= 1; loadEntries(); }
    });
    $("next").addEventListener("click", function () {
      var pages = Math.max(1, Math.ceil(state.total / state.pageSize));
      if (state.page < pages) { state.page += 1; loadEntries(); }
    });
    $("import-btn").addEventListener("click", runImport);
    $("backup-btn").addEventListener("click", function () {
      var dir = $("backup-dir").value.trim();
      if (!dir) { return; }
      postJson("/api/backup", { target_dir: dir }, "import-report");
    });
    $("restore-btn").addEventListener("click", function () {
      var dir = $("restore-dir").value.trim();
      var from = $("restore-from").value.trim();
      if (!dir || !from) { return; }
      postJson("/api/restore", { snapshot_path: from, data_dir: dir }, "import-report");
    });
    $("ai-save").addEventListener("click", function () {
      postJson("/api/settings", {
        ai_opt_in: $("ai-toggle").checked,
        ai_model: $("ai-model").value.trim(),
      }, "ai-out", loadSettings);
    });
    $("ai-run").addEventListener("click", function () {
      var prompt = $("ai-prompt").value.trim();
      if (!prompt) { return; }
      postJson("/api/ai/generate", { prompt: prompt }, "ai-out");
    });
    $("note-add").addEventListener("click", addNote);
    $("drawer-close").addEventListener("click", function () {
      $("drawer").hidden = true;
    });
    document.addEventListener("keydown", function (e) {
      if (e.key === "Escape" && !$("drawer").hidden) { $("drawer").hidden = true; }
    });

    loadSettings();
    loadCollections();
    loadEntries();
    setStatus();
  }

  init();
})();
